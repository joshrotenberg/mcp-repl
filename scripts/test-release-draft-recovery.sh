#!/usr/bin/env bash
# Exercise the post-publication draft-recovery trust boundary.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
source_script="$root/scripts/verify-release-draft-recovery.sh"
release_merge_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
source_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
other_sha=cccccccccccccccccccccccccccccccccccccccc
run_id=32617933653
run_attempt=1
artifact_id=9487682171
release_id=375116865
repository=joshrotenberg/mcp-repl

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
fixture_root="$work/repository"
mkdir -p "$work/bin" "$fixture_root/scripts"
cp "$source_script" "$fixture_root/scripts/verify-release-draft-recovery.sh"
recovery="$fixture_root/scripts/verify-release-draft-recovery.sh"
event_file="$work/event.json"
output_file="$work/github-output"
log="$work/operations.log"

cat > "$fixture_root/scripts/discover-release-merge.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'discover\n' >> "${TEST_LOG:?}"
if [[ "${GH_MODE:-valid}" == untrusted_release_merge ]]; then
  echo 'is_release_merge=false' >> "$1"
else
  echo 'is_release_merge=true' >> "$1"
fi
STUB

cat > "$fixture_root/scripts/extract-release-notes.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == 0.3.5 ]]
printf '%s\n' '### Bug Fixes' '' '- **release:** Accept the Windows SBOM classifier (#237)'
STUB

cat > "$fixture_root/scripts/release-targets.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  expected-release-assets)
    [[ "$2" == v0.3.5 ]]
    for number in $(seq -w 1 39); do
      printf 'asset-%s\n' "$number"
    done
    ;;
  rows)
    printf '%s\n' \
      $'x86_64-unknown-linux-gnu\ttar.gz\tmcp-repl' \
      $'aarch64-unknown-linux-gnu\ttar.gz\tmcp-repl' \
      $'x86_64-unknown-linux-musl\ttar.gz\tmcp-repl' \
      $'aarch64-unknown-linux-musl\ttar.gz\tmcp-repl' \
      $'x86_64-apple-darwin\ttar.gz\tmcp-repl' \
      $'aarch64-apple-darwin\ttar.gz\tmcp-repl' \
      $'x86_64-pc-windows-msvc\tzip\tmcp-repl.exe'
    ;;
  container-platforms)
    printf '%s\n' linux/amd64 linux/arm64
    ;;
  *) exit 1 ;;
esac
STUB

cat > "$fixture_root/scripts/verify-release-tag.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'tag\n' >> "${TEST_LOG:?}"
[[ "$1" == v0.3.5 && "$2" == aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ]]
[[ "${GH_MODE:-valid}" != bad_tag ]]
STUB

cat > "$work/bin/git" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
mode=${GH_MODE:-valid}
fixture_root=${TEST_FIXTURE_ROOT:?}
release_merge=${TEST_RELEASE_MERGE_SHA:?}
source_sha=${TEST_SOURCE_SHA:?}
case "$*" in
  "-C $fixture_root rev-parse HEAD")
    if [[ "$mode" == wrong_checkout ]]; then
      printf '%s\n' "${TEST_OTHER_SHA:?}"
    else
      printf '%s\n' "$source_sha"
    fi
    ;;
  "-C $fixture_root cat-file -e ${release_merge}^{commit}")
    [[ "$mode" != missing_release_merge ]]
    ;;
  "-C $fixture_root merge-base --is-ancestor $release_merge $source_sha")
    [[ "$mode" != unrelated_release_merge ]]
    ;;
  "-C $fixture_root diff --no-renames --name-only -z $release_merge $source_sha --")
    if [[ "$mode" == changed_product ]]; then
      printf 'Cargo.toml\0'
    else
      printf '%s\0' \
        '.github/workflows/ci.yml' \
        '.github/workflows/release-draft-recovery.yml' \
        'docs/releases.md' \
        'scripts/publish-release.sh' \
        'scripts/test-publish-release.sh' \
        'scripts/test-release-draft-recovery.sh' \
        'scripts/test-release-workflow.sh' \
        'scripts/verify-release-draft-recovery.sh' \
        'scripts/verify-release.sh'
    fi
    ;;
  "-C $fixture_root show ${release_merge}:Cargo.toml")
    printf '%s\n' '[package]' 'name = "mcp-repl"' 'version = "0.3.5"'
    ;;
  *)
    echo "unexpected git call: $*" >&2
    exit 1
    ;;
esac
STUB

cat > "$work/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
mode=${GH_MODE:-valid}
repository=${TEST_REPOSITORY:?}
run_id=${TEST_RUN_ID:?}
run_attempt=${TEST_RUN_ATTEMPT:?}
release_merge=${TEST_RELEASE_MERGE_SHA:?}
artifact_id=${TEST_ARTIFACT_ID:?}
release_id=${TEST_RELEASE_ID:?}
tag=v0.3.5

success_names=(
  'Verify release merge'
  'Verify package without credentials'
  'Attempt locked crate publication'
  'Verify published crate identity'
  'Publish immutable binary release / Verify immutable release source'
  'Publish immutable binary release / Build and package every target / Validate release target manifest'
  'Publish immutable binary release / Build and package every target / aarch64-unknown-linux-musl'
  'Publish immutable binary release / Build and package every target / x86_64-unknown-linux-gnu'
  'Publish immutable binary release / Build and package every target / x86_64-apple-darwin'
  'Publish immutable binary release / Build and package every target / x86_64-unknown-linux-musl'
  'Publish immutable binary release / Build and package every target / aarch64-apple-darwin'
  'Publish immutable binary release / Build and package every target / aarch64-unknown-linux-gnu'
  'Publish immutable binary release / Build and package every target / x86_64-pc-windows-msvc'
  'Publish immutable binary release / SBOM and attest aarch64-apple-darwin'
  'Publish immutable binary release / SBOM and attest x86_64-apple-darwin'
  'Publish immutable binary release / SBOM and attest x86_64-unknown-linux-musl'
  'Publish immutable binary release / SBOM and attest x86_64-unknown-linux-gnu'
  'Publish immutable binary release / SBOM and attest aarch64-unknown-linux-gnu'
  'Publish immutable binary release / SBOM and attest aarch64-unknown-linux-musl'
  'Publish immutable binary release / SBOM and attest x86_64-pc-windows-msvc'
  'Publish immutable binary release / Stage attested container platforms / Validate container build inputs'
  'Publish immutable binary release / Stage attested container platforms / linux/arm64'
  'Publish immutable binary release / Stage attested container platforms / linux/amd64'
  'Publish immutable binary release / Assemble and attest the container index'
  'Publish immutable binary release / Assemble the canonical release set'
  'Publish immutable binary release / Anonymous prepublication container smoke'
  'Publish immutable binary release / Publish the immutable container version'
)
failure_name='Publish immutable binary release / Publish the complete immutable GitHub release'
skipped_names=(
  'Publish immutable binary release / Reconcile the mutable latest container alias'
  'Publish immutable binary release / Anonymous public release smoke'
)

emit_job() {
  local name=$1 conclusion=$2 step_name=$3 step_conclusion=$4
  local head=$release_merge workflow='Release publish' status=completed attempt=$run_attempt
  [[ "$mode" != wrong_job_source ]] || head=${TEST_OTHER_SHA:?}
  [[ "$mode" != wrong_job_workflow ]] || workflow='Other workflow'
  [[ "$mode" != wrong_job_attempt ]] || attempt=2
  [[ "$mode" != unfinished_job ]] || status=in_progress
  if [[ "$conclusion" == skipped ]]; then
    steps='[]'
  else
    steps=$(jq -cn \
      --arg name "$step_name" \
      --arg conclusion "$step_conclusion" \
      '[{name:$name,status:"completed",conclusion:$conclusion}]')
  fi
  jq -cn \
    --arg name "$name" \
    --arg conclusion "$conclusion" \
    --arg head "$head" \
    --arg workflow "$workflow" \
    --arg status "$status" \
    --argjson run_id "$run_id" \
    --argjson attempt "$attempt" \
    --argjson steps "$steps" '{
      name:$name,run_id:$run_id,run_attempt:$attempt,head_sha:$head,
      workflow_name:$workflow,status:$status,conclusion:$conclusion,steps:$steps
    }'
}

case "$*" in
  "api graphql "*)
    printf 'graph\n' >> "${TEST_LOG:?}"
    [[ "$mode" != release_view_failure ]] || exit 1
    view_id=$release_id
    view_tag=$tag
    [[ "$mode" != wrong_release_id ]] || view_id=999
    [[ "$mode" != wrong_release_tag ]] || view_tag=v9.9.9
    if [[ "$mode" == partial_graphql_errors ]]; then
      jq -cn --arg tag "$view_tag" --argjson id "$view_id" \
        '{data:{repository:{release:{databaseId:$id,tagName:$tag}}},errors:[{message:"partial failure"}]}'
    else
      jq -cn --arg tag "$view_tag" --argjson id "$view_id" \
        '{data:{repository:{release:{databaseId:$id,tagName:$tag}}}}'
    fi
    ;;
  "api repos/$repository/actions/runs/$run_id/attempts/$run_attempt")
    printf 'run\n' >> "${TEST_LOG:?}"
    [[ "$mode" != run_api_failure ]] || exit 1
    head=$release_merge
    conclusion=failure
    path=.github/workflows/release-publish.yml
    [[ "$mode" != wrong_run_source ]] || head=${TEST_OTHER_SHA:?}
    [[ "$mode" != successful_run ]] || conclusion=success
    [[ "$mode" != wrong_run_path ]] || path=.github/workflows/ci.yml
    jq -cn \
      --arg repository "$repository" \
      --arg head "$head" \
      --arg conclusion "$conclusion" \
      --arg path "$path" \
      --argjson run_id "$run_id" \
      --argjson run_attempt "$run_attempt" '{
        id:$run_id,run_attempt:$run_attempt,event:"push",status:"completed",
        conclusion:$conclusion,head_branch:"main",head_sha:$head,path:$path,
        html_url:("https://github.com/"+$repository+"/actions/runs/"+($run_id|tostring)),
        repository:{full_name:$repository},head_repository:{full_name:$repository},
        actor:{login:"joshrotenberg",type:"User"},
        triggering_actor:{login:"joshrotenberg",type:"User"}
      }'
    ;;
  "api repos/$repository/actions/runs/$run_id/attempts/$run_attempt/jobs?per_page=100")
    printf 'jobs\n' >> "${TEST_LOG:?}"
    [[ "$mode" != jobs_api_failure ]] || exit 1
    jobs_file=$(mktemp)
    for name in "${success_names[@]}"; do
      step='Complete job'
      case "$name" in
        'Verify published crate identity')
          step='Reconcile the exact crates.io package'
          ;;
        'Publish immutable binary release / Assemble the canonical release set')
          step='Preserve the exact canonical release set'
          ;;
        'Publish immutable binary release / Publish the immutable container version')
          step='Publish or verify the exact version image'
          ;;
      esac
      if [[ "$mode" == success_job_failed && "$name" == 'Verify published crate identity' ]]; then
        emit_job "$name" failure "$step" failure >> "$jobs_file"
      else
        emit_job "$name" success "$step" success >> "$jobs_file"
      fi
    done
    failed_step='Create the tag, stage the exact set, and finalize once'
    [[ "$mode" != wrong_failed_step ]] || failed_step='Other failure'
    failure_conclusion=failure
    [[ "$mode" != missing_failure ]] || failure_conclusion=success
    emit_job "$failure_name" "$failure_conclusion" "$failed_step" \
      "$failure_conclusion" >> "$jobs_file"
    for name in "${skipped_names[@]}"; do
      emit_job "$name" skipped '' '' >> "$jobs_file"
    done
    if [[ "$mode" == extra_job ]]; then
      emit_job 'Unexpected job' success 'Complete job' success >> "$jobs_file"
    fi
    jq -sc '{total_count:length,jobs:.}' "$jobs_file"
    rm -f "$jobs_file"
    ;;
  "api repos/$repository/actions/runs/$run_id/artifacts?per_page=100")
    printf 'artifacts\n' >> "${TEST_LOG:?}"
    [[ "$mode" != artifacts_api_failure ]] || exit 1
    expired=false
    artifact_head=$release_merge
    artifact_name="release-supply-$run_attempt"
    [[ "$mode" != expired_artifact ]] || expired=true
    [[ "$mode" != wrong_artifact_source ]] || artifact_head=${TEST_OTHER_SHA:?}
    [[ "$mode" != missing_artifact ]] || artifact_name=other-artifact
    artifact=$(jq -cn \
      --arg name "$artifact_name" \
      --arg head "$artifact_head" \
      --argjson id "$artifact_id" \
      --argjson run_id "$run_id" \
      --argjson expired "$expired" '{
        id:$id,name:$name,expired:$expired,size_in_bytes:57715662,
        digest:"sha256:1da9c92608be1cf39557b6f10bdccebd0ce8bd901b8343950f6a19219cd1cb46",
        workflow_run:{id:$run_id,head_branch:"main",head_sha:$head}
      }')
    if [[ "$mode" == duplicate_release_artifact ]]; then
      other=$(jq -cn \
        --arg head "$artifact_head" \
        --argjson run_id "$run_id" '{
          id:999,name:"release-supply-2",expired:false,size_in_bytes:1,
          digest:"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          workflow_run:{id:$run_id,head_branch:"main",head_sha:$head}
        }')
      jq -cn --argjson first "$artifact" --argjson second "$other" \
        '{total_count:2,artifacts:[$first,$second]}'
    else
      jq -cn --argjson artifact "$artifact" \
        '{total_count:1,artifacts:[$artifact]}'
    fi
    ;;
  "api --paginate --slurp repos/$repository/releases?per_page=100")
    printf 'release-list\n' >> "${TEST_LOG:?}"
    [[ "$mode" != release_list_failure ]] || exit 1
    first=$(jq -cn --arg tag "$tag" --argjson id "$release_id" \
      '{id:$id,tag_name:$tag}')
    if [[ "$mode" == duplicate_release ]]; then
      second=$(jq -cn --arg tag "$tag" '{id:999,tag_name:$tag}')
      jq -cn --argjson first "$first" --argjson second "$second" \
        '[[$first,$second]]'
    elif [[ "$mode" == malformed_release_page_entry ]]; then
      jq -cn --argjson first "$first" '[[$first,"malformed"]]'
    elif [[ "$mode" == malformed_release_object ]]; then
      jq -cn --argjson first "$first" '[[$first,{id:999}]]'
    else
      jq -cn --argjson first "$first" '[[$first]]'
    fi
    ;;
  "api repos/$repository/releases/$release_id")
    printf 'release\n' >> "${TEST_LOG:?}"
    [[ "$mode" != release_api_failure ]] || exit 1
    body=$'### Bug Fixes\n\n- **release:** Accept the Windows SBOM classifier (#237)\n'
    author=github-actions[bot]
    draft=true
    immutable=false
    published=null
    [[ "$mode" != wrong_notes ]] || body='wrong notes'
    [[ "$mode" != foreign_draft ]] || author=octocat
    if [[ "$mode" == valid_published || "$mode" == published_incomplete ]]; then
      draft=false
      immutable=true
      published='"2026-08-23T05:00:00Z"'
    fi
    jq -cn \
      --arg tag "$tag" \
      --arg body "$body" \
      --arg author "$author" \
      --argjson id "$release_id" \
      --argjson draft "$draft" \
      --argjson immutable "$immutable" \
      --argjson published "$published" '{
        id:$id,tag_name:$tag,name:$tag,body:$body,target_commitish:"main",
        draft:$draft,prerelease:false,immutable:$immutable,published_at:$published,
        author:{login:$author,type:(if $author=="github-actions[bot]" then "Bot" else "User" end)}
      }'
    ;;
  "api --paginate --slurp repos/$repository/releases/$release_id/assets?per_page=100")
    printf 'assets\n' >> "${TEST_LOG:?}"
    if [[ "$mode" == assets_api_failure ]]; then
      exit 1
    elif [[ "$mode" == unexpected_asset ]]; then
      printf '[[{"id":1,"name":"foreign","size":1,"state":"uploaded"}]]\n'
    elif [[ "$mode" == duplicate_asset_id ]]; then
      printf '[[{"id":1,"name":"asset-01","size":1,"state":"uploaded"},{"id":1,"name":"asset-02","size":1,"state":"uploaded"}]]\n'
    elif [[ "$mode" == valid_published ]]; then
      assets_file=$(mktemp)
      for number in $(seq -w 1 39); do
        jq -cn --arg name "asset-$number" --argjson id "${number#0}" \
          '{id:$id,name:$name,size:1,state:"uploaded"}' >> "$assets_file"
      done
      jq -sc '[.]' "$assets_file"
      rm -f "$assets_file"
    else
      printf '[[]]\n'
    fi
    ;;
  *)
    echo "unexpected gh call: $*" >&2
    exit 1
    ;;
esac
STUB

chmod +x \
  "$recovery" \
  "$fixture_root/scripts/discover-release-merge.sh" \
  "$fixture_root/scripts/extract-release-notes.sh" \
  "$fixture_root/scripts/release-targets.sh" \
  "$fixture_root/scripts/verify-release-tag.sh" \
  "$work/bin/git" \
  "$work/bin/gh"

write_event() {
  local mode=$1 action=release_draft_recovery event_repository=$repository
  local default_branch=main sender=joshrotenberg schema=1 extra=
  local payload_release=$release_merge_sha payload_run_id=$run_id
  local payload_attempt=$run_attempt
  case "$mode" in
    wrong_action) action=release_publish_recovery ;;
    wrong_event_repository) event_repository=other/project ;;
    wrong_default_branch) default_branch=develop ;;
    wrong_sender) sender=octocat ;;
    wrong_schema) schema=2 ;;
    malformed_release_sha) payload_release=short ;;
    string_run_id) payload_run_id="\"$run_id\"" ;;
    string_run_attempt) payload_attempt="\"$run_attempt\"" ;;
    extra_payload) extra=',"source_sha":"dddddddddddddddddddddddddddddddddddddddd"' ;;
  esac
  printf '{"action":"%s","repository":{"full_name":"%s","default_branch":"%s"},"sender":{"login":"%s","type":"User"},"client_payload":{"schema_version":%s,"release_merge_sha":"%s","run_id":%s,"run_attempt":%s%s}}\n' \
    "$action" "$event_repository" "$default_branch" "$sender" "$schema" \
    "$payload_release" "$payload_run_id" "$payload_attempt" "$extra" > "$event_file"
}

failures=0
check() {
  local name=$1 mode=$2 phase=$3 want_status=$4 want_text=$5
  local event_name=${6:-repository_dispatch} event_ref=${7:-refs/heads/main}
  local current_actor=joshrotenberg output status
  write_event "$mode"
  : > "$output_file"
  : > "$log"
  [[ "$mode" != wrong_current_actor ]] || current_actor=octocat
  set +e
  output=$(PATH="$work/bin:$PATH" \
    GH_MODE="$mode" \
    GITHUB_ACTOR="$current_actor" \
    GITHUB_EVENT_NAME="$event_name" \
    GITHUB_EVENT_PATH="$event_file" \
    GITHUB_REF="$event_ref" \
    GITHUB_REPOSITORY="$repository" \
    GITHUB_SERVER_URL=https://github.com \
    GITHUB_SHA="$source_sha" \
    GITHUB_TRIGGERING_ACTOR="$current_actor" \
    TEST_ARTIFACT_ID="$artifact_id" \
    TEST_FIXTURE_ROOT="$fixture_root" \
    TEST_LOG="$log" \
    TEST_OTHER_SHA="$other_sha" \
    TEST_RELEASE_ID="$release_id" \
    TEST_RELEASE_MERGE_SHA="$release_merge_sha" \
    TEST_REPOSITORY="$repository" \
    TEST_RUN_ATTEMPT="$run_attempt" \
    TEST_RUN_ID="$run_id" \
    TEST_SOURCE_SHA="$source_sha" \
    "$recovery" "$phase" "$output_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" != "$want_status" || "$output" != *"$want_text"* ]]; then
    printf 'FAIL %s: exit %s wanted %s, missing %q\n%s\n' \
      "$name" "$status" "$want_status" "$want_text" "$output" >&2
    failures=$((failures + 1))
    return
  fi
  if [[ "$want_status" == 0 ]]; then
    expected=$'release_merge_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nrun_id=32617933653\nrun_attempt=1\nartifact_id=9487682171\nartifact_digest=sha256:1da9c92608be1cf39557b6f10bdccebd0ce8bd901b8343950f6a19219cd1cb46\nrelease_id=375116865\nrelease_tag=v0.3.5'
    if [[ $(<"$output_file") != "$expected" ]]; then
      printf 'FAIL %s: trusted outputs differ\n%s\n' "$name" "$(<"$output_file")" >&2
      failures=$((failures + 1))
    fi
  elif [[ -s "$output_file" ]]; then
    printf 'FAIL %s: failed verification wrote outputs\n' "$name" >&2
    failures=$((failures + 1))
  fi
  printf 'ok   %s\n' "$name"
}

check "exact failed evidence authorizes draft recovery" valid preflight 0 "Authenticated exact draft recovery"
check "write-scoped reauthentication repeats live evidence without PR reads" valid resume 0 "Authenticated exact draft recovery"
check "an already-published exact release is idempotently recoverable" valid_published resume 0 "Authenticated exact draft recovery"
check "only repository dispatch can recover a draft" valid preflight 2 "Invalid release-draft recovery environment" push
check "draft recovery stays on main" valid preflight 2 "Invalid release-draft recovery environment" repository_dispatch refs/heads/release
check "only the owner can run recovery" wrong_current_actor preflight 2 "Invalid release-draft recovery environment"
check "the dispatch type is exact" wrong_action preflight 1 "malformed"
check "the event repository is exact" wrong_event_repository preflight 1 "malformed"
check "the default branch is exact" wrong_default_branch preflight 1 "malformed"
check "the event sender is exact" wrong_sender preflight 1 "malformed"
check "the payload schema is versioned" wrong_schema preflight 1 "malformed"
check "the release merge is a full SHA" malformed_release_sha preflight 1 "malformed"
check "the run ID remains numeric" string_run_id preflight 1 "malformed"
check "the run attempt remains numeric" string_run_attempt preflight 1 "malformed"
check "extra source selection is refused" extra_payload preflight 1 "malformed"
check "the checkout remains current reviewed code" wrong_checkout preflight 1 "does not match event source"
check "the release merge must still exist" missing_release_merge preflight 1 "not an ancestor"
check "the release merge must remain an ancestor" unrelated_release_merge preflight 1 "not an ancestor"
check "changed product inputs force a fresh version" changed_product preflight 1 "outside the reviewed allowlist"
check "the release merge must remain trusted" untrusted_release_merge preflight 1 "trusted release merge"
check "a failed-run API error is preserved" run_api_failure preflight 1 "Could not read"
check "the run source is exact" wrong_run_source preflight 1 "exact failed Release publish"
check "the run must have failed" successful_run preflight 1 "exact failed Release publish"
check "the run workflow is exact" wrong_run_path preflight 1 "exact failed Release publish"
check "a job API error is preserved" jobs_api_failure preflight 1 "Could not read jobs"
check "every job source is exact" wrong_job_source preflight 1 "exact recoverable"
check "every job attempt is exact" wrong_job_attempt preflight 1 "exact recoverable"
check "every job workflow is exact" wrong_job_workflow preflight 1 "exact recoverable"
check "unfinished jobs are refused" unfinished_job preflight 1 "exact recoverable"
check "a prerequisite failure is refused" success_job_failed preflight 1 "exact recoverable"
check "the publication failure is required" missing_failure preflight 1 "exact recoverable"
check "the exact publication step must fail" wrong_failed_step preflight 1 "exact recoverable"
check "extra jobs are refused" extra_job preflight 1 "exact recoverable"
check "an artifact API error is preserved" artifacts_api_failure preflight 1 "Could not read artifacts"
check "the canonical artifact is required" missing_artifact preflight 1 "one exact unexpired"
check "an expired canonical artifact is refused" expired_artifact preflight 1 "one exact unexpired"
check "the artifact source is exact" wrong_artifact_source preflight 1 "one exact unexpired"
check "multiple canonical artifacts are refused" duplicate_release_artifact preflight 1 "one exact unexpired"
check "draft discovery failures are preserved" release_view_failure preflight 1 "Could not discover"
check "the discovered tag is exact" wrong_release_tag preflight 1 "invalid GraphQL recovery release identity"
check "partial GraphQL errors are refused" partial_graphql_errors preflight 1 "invalid GraphQL recovery release identity"
check "release-list failures are preserved" release_list_failure preflight 1 "Could not enumerate"
check "malformed entries anywhere in release pages are refused" malformed_release_page_entry preflight 1 "do not bind one exact"
check "malformed release objects are refused before tag filtering" malformed_release_object preflight 1 "do not bind one exact"
check "GraphQL and REST release identities must agree" wrong_release_id preflight 1 "do not bind one exact"
check "duplicate same-tag releases are refused" duplicate_release preflight 1 "do not bind one exact"
check "release API failures are preserved" release_api_failure preflight 1 "Could not read exact"
check "canonical notes are required byte for byte" wrong_notes preflight 1 "exact canonical notes"
check "a foreign draft is refused" foreign_draft preflight 1 "not the trusted resumable draft"
check "unexpected draft assets are refused" unexpected_asset preflight 1 "unsafe or noncanonical"
check "duplicate draft asset IDs are refused" duplicate_asset_id preflight 1 "unsafe or noncanonical"
check "a published release must already have every asset" published_incomplete preflight 1 "unsafe or noncanonical"
check "tag verification is required" bad_tag preflight 1 "does not bind the recovered source"

if [[ "$failures" -ne 0 ]]; then
  printf '%s release draft recovery tests failed\n' "$failures" >&2
  exit 1
fi
printf 'all release draft recovery tests passed\n'
