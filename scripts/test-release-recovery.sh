#!/usr/bin/env bash
# Exercise the exceptional source-publication recovery trust boundary.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
source_script="$root/scripts/verify-release-recovery.sh"
release_merge_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
source_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
other_sha=cccccccccccccccccccccccccccccccccccccccc
run_id=98
run_attempt=1
repository=joshrotenberg/mcp-repl

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
fixture_root="$work/repository"
mkdir -p "$work/bin" "$fixture_root/scripts"
cp "$source_script" "$fixture_root/scripts/verify-release-recovery.sh"
recovery="$fixture_root/scripts/verify-release-recovery.sh"
event_file="$work/event.json"
output_file="$work/github-output"
log="$work/operations.log"

cat > "$fixture_root/scripts/discover-release-merge.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ $# -ne 2 || "$2" != "${TEST_RELEASE_MERGE_SHA:?}" ]]; then
  echo "recovery did not revalidate the payload's exact release merge" >&2
  exit 1
fi
printf 'discover\n' >> "${TEST_LOG:?}"
if [[ "${GH_MODE:-valid}" == untrusted_release_merge ]]; then
  echo "is_release_merge=false" >> "$1"
  echo "This commit did not merge a release-plz PR"
else
  echo "is_release_merge=true" >> "$1"
  echo "This commit merged exactly one trusted release-plz PR"
fi
STUB

cat > "$work/bin/git" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
mode=${GH_MODE:-valid}
root=${TEST_RECOVERY_ROOT:?}
release_merge=${TEST_RELEASE_MERGE_SHA:?}
source_sha=${TEST_SOURCE_SHA:?}
case "$*" in
  "-C $root rev-parse HEAD")
    if [[ "$mode" == wrong_checkout ]]; then
      printf '%s\n' "${TEST_OTHER_SHA:?}"
    else
      printf '%s\n' "$source_sha"
    fi
    ;;
  "-C $root cat-file -e ${release_merge}^{commit}")
    [[ "$mode" != missing_release_commit ]]
    ;;
  "-C $root merge-base --is-ancestor $release_merge $source_sha")
    [[ "$mode" != unrelated_release_merge ]]
    ;;
  "-C $root diff --no-renames --name-only -z $release_merge $source_sha --")
    if [[ "$mode" == changed_release_input ]]; then
      printf 'Cargo.toml\0'
    elif [[ "$mode" == renamed_release_input ]]; then
      printf 'src/old.rs\0src/new.rs\0'
    else
      printf '%s\0' \
        '.github/workflows/release-publish.yml' \
        'scripts/select-run-artifacts.sh' \
        'scripts/test-select-run-artifacts.sh' \
        'scripts/verify-release-recovery.sh' \
        'scripts/test-release-recovery.sh'
    fi
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
call=$*
case "$call" in
  "api repos/$repository/actions/runs/$run_id/attempts/$run_attempt")
    printf 'run\n' >> "${TEST_LOG:?}"
    [[ "$mode" != run_api_failure ]] || exit 1
    if [[ "$mode" == malformed_run ]]; then
      printf '{}\n'
      exit 0
    fi
    event=push
    status=completed
    conclusion=failure
    branch=main
    head=$release_merge
    path=.github/workflows/release-publish.yml
    repo=$repository
    head_repo=$repository
    response_run_id=$run_id
    response_attempt=$run_attempt
    url="https://github.com/$repository/actions/runs/$run_id"
    actor=joshrotenberg
    actor_type=User
    triggering_actor=joshrotenberg
    triggering_actor_type=User
    case "$mode" in
      wrong_run_event) event=workflow_dispatch ;;
      unfinished_run) status=in_progress; conclusion='' ;;
      successful_run) conclusion=success ;;
      wrong_run_branch) branch=release-plz-v1 ;;
      wrong_run_head) head=${TEST_OTHER_SHA:?} ;;
      wrong_run_path) path=.github/workflows/ci.yml ;;
      wrong_run_repository) repo=other/project ;;
      wrong_run_head_repository) head_repo=other/project ;;
      wrong_run_id) response_run_id=99 ;;
      wrong_run_attempt) response_attempt=2 ;;
      wrong_run_url) url=https://example.com/untrusted ;;
      wrong_run_actor) actor=octocat ;;
      wrong_run_triggering_actor) triggering_actor=octocat ;;
    esac
    if [[ -n "$conclusion" ]]; then
      conclusion_json="\"$conclusion\""
    else
      conclusion_json=null
    fi
    printf '{"id":%s,"run_attempt":%s,"event":"%s","status":"%s","conclusion":%s,"head_branch":"%s","head_sha":"%s","path":"%s","html_url":"%s","repository":{"full_name":"%s"},"head_repository":{"full_name":"%s"},"actor":{"login":"%s","type":"%s"},"triggering_actor":{"login":"%s","type":"%s"}}\n' \
      "$response_run_id" "$response_attempt" "$event" "$status" "$conclusion_json" \
      "$branch" "$head" "$path" "$url" "$repo" "$head_repo" \
      "$actor" "$actor_type" "$triggering_actor" "$triggering_actor_type"
    ;;
  "api repos/$repository/actions/runs/$run_id/attempts/$run_attempt/jobs?per_page=100")
    printf 'jobs\n' >> "${TEST_LOG:?}"
    [[ "$mode" != jobs_api_failure ]] || exit 1
    if [[ "$mode" == malformed_jobs ]]; then
      printf '{}\n'
      exit 0
    fi
    job_head=$release_merge
    job_attempt=$run_attempt
    job_workflow='Release publish'
    job_status=completed
    publish_conclusion=success
    reconcile_conclusion=failure
    reconcile_step=failure
    reconcile_step_name='Reconcile the exact crates.io package'
    binary_name='Publish immutable binary release'
    total=5
    [[ "$mode" != wrong_job_source ]] || job_head=${TEST_OTHER_SHA:?}
    if [[ "$mode" == wrong_job_topology ]]; then
      reconcile_conclusion=success
      reconcile_step=success
    fi
    [[ "$mode" != wrong_job_attempt ]] || job_attempt=2
    [[ "$mode" != wrong_job_workflow ]] || job_workflow='Other workflow'
    [[ "$mode" != unfinished_job ]] || job_status=in_progress
    [[ "$mode" != failed_publish_job ]] || publish_conclusion=failure
    [[ "$mode" != wrong_reconcile_step ]] || reconcile_step_name='Other reconciliation'
    [[ "$mode" != missing_expected_job ]] || binary_name='Unexpected job'
    [[ "$mode" != duplicate_expected_job ]] || binary_name='Verify release merge'
    [[ "$mode" != extra_job_count ]] || total=6
    printf '{"total_count":%s,"jobs":[' "$total"
    printf '{"name":"Verify release merge","run_id":%s,"run_attempt":%s,"head_sha":"%s","workflow_name":"%s","status":"%s","conclusion":"success","steps":[{"name":"Match exactly one trusted release PR","status":"completed","conclusion":"success"}]},' \
      "$run_id" "$job_attempt" "$job_head" "$job_workflow" "$job_status"
    printf '{"name":"Verify package without credentials","run_id":%s,"run_attempt":%s,"head_sha":"%s","workflow_name":"%s","status":"%s","conclusion":"success","steps":[{"name":"Verify exact package without a registry credential","status":"completed","conclusion":"success"}]},' \
      "$run_id" "$job_attempt" "$job_head" "$job_workflow" "$job_status"
    printf '{"name":"Attempt locked crate publication","run_id":%s,"run_attempt":%s,"head_sha":"%s","workflow_name":"%s","status":"%s","conclusion":"%s","steps":[{"name":"Upload exact locked source crate without build execution","status":"completed","conclusion":"success"}]},' \
      "$run_id" "$job_attempt" "$job_head" "$job_workflow" "$job_status" "$publish_conclusion"
    printf '{"name":"Verify published crate identity","run_id":%s,"run_attempt":%s,"head_sha":"%s","workflow_name":"%s","status":"%s","conclusion":"%s","steps":[{"name":"%s","status":"completed","conclusion":"%s"}]},' \
      "$run_id" "$job_attempt" "$job_head" "$job_workflow" "$job_status" \
      "$reconcile_conclusion" "$reconcile_step_name" "$reconcile_step"
    printf '{"name":"%s","run_id":%s,"run_attempt":%s,"head_sha":"%s","workflow_name":"%s","status":"%s","conclusion":"skipped","steps":[]}]}\n' \
      "$binary_name" "$run_id" "$job_attempt" "$job_head" "$job_workflow" "$job_status"
    ;;
  *)
    echo "unexpected gh call: $call" >&2
    exit 1
    ;;
esac
STUB

chmod +x "$recovery" "$fixture_root/scripts/discover-release-merge.sh" \
  "$work/bin/git" "$work/bin/gh"

write_event() {
  local mode=$1
  local action=release_publish_recovery event_repository=$repository
  local default_branch=main schema=1 payload_release=$release_merge_sha
  local payload_run_id=$run_id payload_attempt=$run_attempt
  local sender=joshrotenberg sender_type=User
  local run_id_json attempt_json extra_payload=
  case "$mode" in
    wrong_action) action=release_validation ;;
    wrong_event_repository) event_repository=other/project ;;
    wrong_default_branch) default_branch=develop ;;
    wrong_schema) schema=2 ;;
    malformed_release_sha) payload_release=short ;;
    string_run_id) payload_run_id="\"$run_id\"" ;;
    string_run_attempt) payload_attempt="\"$run_attempt\"" ;;
    extra_payload) extra_payload=',"source_sha":"dddddddddddddddddddddddddddddddddddddddd"' ;;
    wrong_sender) sender=octocat ;;
  esac
  if [[ "$payload_run_id" == \"* ]]; then
    run_id_json=$payload_run_id
  else
    run_id_json=$payload_run_id
  fi
  if [[ "$payload_attempt" == \"* ]]; then
    attempt_json=$payload_attempt
  else
    attempt_json=$payload_attempt
  fi
  printf '{"action":"%s","repository":{"full_name":"%s","default_branch":"%s"},"sender":{"login":"%s","type":"%s"},"client_payload":{"schema_version":%s,"release_merge_sha":"%s","run_id":%s,"run_attempt":%s%s}}\n' \
    "$action" "$event_repository" "$default_branch" "$sender" "$sender_type" "$schema" "$payload_release" \
    "$run_id_json" "$attempt_json" "$extra_payload" > "$event_file"
  if [[ "$mode" == malformed_event ]]; then
    printf '{}\n' > "$event_file"
  fi
}

failures=0
assert_result() {
  local name=$1 status=$2 want_status=$3 output=$4 want_text=$5
  if [[ "$status" != "$want_status" ]]; then
    printf 'FAIL %s: exit %s, wanted %s\n%s\n' "$name" "$status" "$want_status" "$output" >&2
    failures=$((failures + 1))
    return 1
  fi
  if [[ "$output" != *"$want_text"* ]]; then
    printf 'FAIL %s: output did not mention %q\n%s\n' "$name" "$want_text" "$output" >&2
    failures=$((failures + 1))
    return 1
  fi
  printf 'ok   %s\n' "$name"
}

check_recovery() {
  local name=$1 mode=$2 phase=$3 want_status=$4 want_text=$5
  local expected=${6:-$release_merge_sha} event_name=${7:-repository_dispatch}
  local event_ref=${8:-refs/heads/main} event_sha=${9:-$source_sha}
  local output status current_actor=joshrotenberg
  write_event "$mode"
  : > "$output_file"
  : > "$log"
  args=("$phase" "$output_file")
  [[ "$expected" == none ]] || args+=("$expected")
  [[ "$mode" != wrong_current_actor ]] || current_actor=octocat
  set +e
  output=$(PATH="$work/bin:$PATH" \
    GH_MODE="$mode" \
    GITHUB_EVENT_NAME="$event_name" \
    GITHUB_EVENT_PATH="$event_file" \
    GITHUB_REF="$event_ref" \
    GITHUB_REPOSITORY="$repository" \
    GITHUB_SERVER_URL=https://github.com \
    GITHUB_SHA="$event_sha" \
    GITHUB_ACTOR="$current_actor" \
    GITHUB_TRIGGERING_ACTOR="$current_actor" \
    TEST_LOG="$log" \
    TEST_OTHER_SHA="$other_sha" \
    TEST_RECOVERY_ROOT="$fixture_root" \
    TEST_RELEASE_MERGE_SHA="$release_merge_sha" \
    TEST_REPOSITORY="$repository" \
    TEST_RUN_ATTEMPT="$run_attempt" \
    TEST_RUN_ID="$run_id" \
    TEST_SOURCE_SHA="$source_sha" \
    "$recovery" "${args[@]}" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi
  if [[ "$want_status" == 0 ]]; then
    expected_output=$'is_release_merge=true\nrecovery_release_sha='"$release_merge_sha"
    if [[ $(<"$output_file") != "$expected_output" ]]; then
      echo "FAIL $name: recovery outputs were not exact" >&2
      failures=$((failures + 1))
    fi
    expected_log=$'discover\nrun\njobs'
    if [[ $(<"$log") != "$expected_log" ]]; then
      echo "FAIL $name: recovery checks did not execute in the expected order" >&2
      failures=$((failures + 1))
    fi
  elif [[ -s "$output_file" ]]; then
    echo "FAIL $name: a failed recovery wrote trusted outputs" >&2
    failures=$((failures + 1))
  fi
}

check_recovery "an exact failed run authorizes current-main recovery" valid pre-publish 0 "Authenticated recovery" none
check_recovery "post-publication rechecks preserve the same authorization" valid post-publish 0 "Authenticated recovery"
check_recovery "pre-publication accepts no externally supplied authorization" valid pre-publish 2 "usage:" "$release_merge_sha"
check_recovery "post-publication requires the previously checked authorization" valid post-publish 2 "usage:" none
check_recovery "the reusable stage rejects changed authorization" valid post-publish 1 "changed its trusted release merge" "$other_sha"
check_recovery "only repository dispatch can request recovery" valid pre-publish 2 "Invalid release-recovery environment" none push
check_recovery "recovery must remain on main" valid pre-publish 2 "Invalid release-recovery environment" none repository_dispatch refs/heads/release
check_recovery "the checkout must match current default-branch code" wrong_checkout pre-publish 1 "does not match event source" none
check_recovery "a malformed event is refused" malformed_event pre-publish 1 "malformed" none
check_recovery "the dispatch action is exact" wrong_action pre-publish 1 "malformed" none
check_recovery "another event repository is refused" wrong_event_repository pre-publish 1 "malformed" none
check_recovery "another default branch is refused" wrong_default_branch pre-publish 1 "malformed" none
check_recovery "only the repository owner can dispatch recovery" wrong_current_actor pre-publish 2 "Invalid release-recovery environment" none
check_recovery "the event sender must be the repository owner" wrong_sender pre-publish 1 "malformed" none
check_recovery "the payload schema is versioned" wrong_schema pre-publish 1 "malformed" none
check_recovery "the release merge must be a full lowercase SHA" malformed_release_sha pre-publish 1 "malformed" none
check_recovery "the failed run ID must be numeric" string_run_id pre-publish 1 "malformed" none
check_recovery "the failed run attempt must be numeric" string_run_attempt pre-publish 1 "malformed" none
check_recovery "extra source-selection payload is refused" extra_payload pre-publish 1 "malformed" none
check_recovery "the failed merge must still exist" missing_release_commit pre-publish 1 "not an ancestor" none
check_recovery "the failed merge must be an ancestor" unrelated_release_merge pre-publish 1 "not an ancestor" none
check_recovery "changed package inputs require a fresh version" changed_release_input pre-publish 1 "outside the reviewed allowlist" none
check_recovery "renamed package inputs require a fresh version" renamed_release_input pre-publish 1 "outside the reviewed allowlist" none
check_recovery "the original merge is reauthenticated" untrusted_release_merge pre-publish 1 "does not identify exactly one trusted" none
check_recovery "a failed-run API error is preserved" run_api_failure pre-publish 1 "Could not read the failed" none
check_recovery "malformed failed-run data is refused" malformed_run pre-publish 1 "does not identify the exact" none
check_recovery "only a failed push run is eligible" wrong_run_event pre-publish 1 "does not identify the exact" none
check_recovery "an unfinished run is ineligible" unfinished_run pre-publish 1 "does not identify the exact" none
check_recovery "a successful run is ineligible" successful_run pre-publish 1 "does not identify the exact" none
check_recovery "the failed run must belong to main" wrong_run_branch pre-publish 1 "does not identify the exact" none
check_recovery "the failed run must bind the release merge" wrong_run_head pre-publish 1 "does not identify the exact" none
check_recovery "the failed run must be Release publish" wrong_run_path pre-publish 1 "does not identify the exact" none
check_recovery "the failed run repository is exact" wrong_run_repository pre-publish 1 "does not identify the exact" none
check_recovery "the failed run head repository is exact" wrong_run_head_repository pre-publish 1 "does not identify the exact" none
check_recovery "the failed run ID is exact" wrong_run_id pre-publish 1 "does not identify the exact" none
check_recovery "the failed run attempt is exact" wrong_run_attempt pre-publish 1 "does not identify the exact" none
check_recovery "the failed run URL is canonical" wrong_run_url pre-publish 1 "does not identify the exact" none
check_recovery "the failed run actor is trusted" wrong_run_actor pre-publish 1 "does not identify the exact" none
check_recovery "the failed run triggering actor is trusted" wrong_run_triggering_actor pre-publish 1 "does not identify the exact" none
check_recovery "a failed-jobs API error is preserved" jobs_api_failure pre-publish 1 "Could not read jobs" none
check_recovery "malformed failed-job data is refused" malformed_jobs pre-publish 1 "exact recoverable" none
check_recovery "unexpected failed jobs are refused" extra_job_count pre-publish 1 "exact recoverable" none
check_recovery "a missing expected job is refused" missing_expected_job pre-publish 1 "exact recoverable" none
check_recovery "a duplicate expected job is refused" duplicate_expected_job pre-publish 1 "exact recoverable" none
check_recovery "jobs from another source are refused" wrong_job_source pre-publish 1 "exact recoverable" none
check_recovery "jobs from another attempt are refused" wrong_job_attempt pre-publish 1 "exact recoverable" none
check_recovery "jobs from another workflow are refused" wrong_job_workflow pre-publish 1 "exact recoverable" none
check_recovery "unfinished jobs are refused" unfinished_job pre-publish 1 "exact recoverable" none
check_recovery "a failed publication job is not the recoverable topology" failed_publish_job pre-publish 1 "exact recoverable" none
check_recovery "a renamed reconciliation step is refused" wrong_reconcile_step pre-publish 1 "exact recoverable" none
check_recovery "another failure topology is refused" wrong_job_topology pre-publish 1 "exact recoverable" none
if [[ "$failures" -ne 0 ]]; then
  echo "$failures release recovery check(s) failed" >&2
  exit 1
fi
echo "all release recovery checks passed"
