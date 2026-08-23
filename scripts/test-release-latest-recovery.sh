#!/usr/bin/env bash
# Exercise the one-shot v0.3.5 latest-recovery and anonymous-smoke boundaries.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
source_verifier="$root/scripts/verify-release-latest-recovery.sh"
source_smoke="$root/scripts/smoke-release-latest-recovery.sh"
release_merge_sha=7b51781718975772d96006f167887adb877618e7
source_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
other_sha=cccccccccccccccccccccccccccccccccccccccc
run_id=32617933653
run_attempt=2
current_run_id=90000000001
current_run_attempt=1
release_id=375116865
record_asset_id=525939539
repository=joshrotenberg/mcp-repl
tag=v0.3.5
record_digest=sha256:746b2df14a1a6d3cc8779210c3f5dd5e27691853ce231cb7842fd8b704427325
manifest_digest=sha256:3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c
platforms='[{"platform":"linux/amd64","runnable_digest":"sha256:8f31eb764fea23b4491b4aa08566949a6be5fde0b3aa7c8b529b0ec0559806a9"},{"platform":"linux/arm64","runnable_digest":"sha256:14fb0771b1e2333c492fde114920eaa2149f823771d27fd95394651261d8e7d8"}]'
buildkit_image=docker.io/moby/buildkit:v0.32.2@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
buildx_asset=buildx-v0.36.1.linux-amd64
buildx_sha256=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
fixture_root="$work/repository"
mkdir -p "$work/bin" "$fixture_root/scripts"
cp "$source_verifier" "$source_smoke" "$root/scripts/container-runnable-mapping.jq" \
  "$fixture_root/scripts/"
verifier="$fixture_root/scripts/verify-release-latest-recovery.sh"
smoke="$fixture_root/scripts/smoke-release-latest-recovery.sh"
event_file="$work/event.json"
output_file="$work/github-output"
log="$work/operations.log"
record_fixture="$work/release-record.json"
spdx_fixture="$work/container.spdx.json"
provenance_fixture="$work/container.provenance.sigstore.json"
sbom_bundle_fixture="$work/container.sbom.sigstore.json"
index_fixture="$work/index.json"

make_padded_json() {
  local destination=$1 size=$2 filter=$3 base current padding
  base="$work/padded-base.json"
  jq -cnS --arg padding '' "$filter" > "$base"
  current=$(wc -c < "$base" | tr -d '[:space:]')
  padding=$((size - current))
  [[ "$padding" -ge 0 ]]
  jq -cnS --arg padding "$(printf '%*s' "$padding" '' | tr ' ' x)" \
    "$filter" > "$destination"
  [[ $(wc -c < "$destination" | tr -d '[:space:]') == "$size" ]]
}

# shellcheck disable=SC2016
make_padded_json "$record_fixture" 7616 '
  def asset_name($number):
    "asset-" + (if $number < 10 then "0" else "" end) + ($number | tostring);
  def identity($number): {
    name:asset_name($number),
    size:1,
    sha256:"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  };
  {
    schema_version:1,
    package:"mcp-repl",
    tag:"v0.3.5",
    version:"0.3.5",
    source_sha:"7b51781718975772d96006f167887adb877618e7",
    source_epoch:1787459270,
    release_targets:{
      name:"release-targets.json",
      sha256:"7517f938a08147aa4dab9cd17748e8eabe429058ce38960431d3966f18335956",
      size:4940
    },
    native:[range(0;7) as $index | {
      target:("target-" + ($index | tostring)),
      binary:"mcp-repl",
      archive:identity(($index * 5) + 1),
      checksum:identity(($index * 5) + 2),
      sbom:identity(($index * 5) + 3),
      attestations:{
        provenance:identity(($index * 5) + 4),
        sbom:identity(($index * 5) + 5)
      },
      padding:(if $index == 0 then $padding else "" end)
    }],
    container:{
      image:"ghcr.io/joshrotenberg/mcp-repl",
      manifest_digest:"sha256:3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c",
      platforms:[
        {platform:"linux/amd64",runnable_digest:"sha256:8f31eb764fea23b4491b4aa08566949a6be5fde0b3aa7c8b529b0ec0559806a9"},
        {platform:"linux/arm64",runnable_digest:"sha256:14fb0771b1e2333c492fde114920eaa2149f823771d27fd95394651261d8e7d8"}
      ],
      sbom:{
        name:"mcp-repl-v0.3.5-container.spdx.json",
        size:3635,
        sha256:"e5cf3b6a397ec1150076700900d183860e143bc54dd9df8b2ea6e63149fcc849"
      },
      attestations:{
        provenance:{
          name:"mcp-repl-v0.3.5-container.provenance.sigstore.json",
          size:10996,
          sha256:"855025d566da00ff9ef19b8d8a6a907f905f9480ceda27fa1a3c4226f5db9211"
        },
        sbom:{
          name:"mcp-repl-v0.3.5-container.sbom.sigstore.json",
          size:13971,
          sha256:"68be24b77e3ccce2ca0a4a8850b75b42fbe3a84a694922e0e85210b2921f592d"
        }
      }
    }
  }
'
# shellcheck disable=SC2016
make_padded_json "$spdx_fixture" 3635 \
  '{dataLicense:"CC0-1.0",padding:$padding,spdxVersion:"SPDX-2.3"}'
awk 'BEGIN { for (i = 0; i < 10996; i++) printf "p" }' > "$provenance_fixture"
awk 'BEGIN { for (i = 0; i < 13971; i++) printf "s" }' > "$sbom_bundle_fixture"

jq -cnS '
  {
    schemaVersion:2,
    mediaType:"application/vnd.oci.image.index.v1+json",
    manifests:[
      {
        mediaType:"application/vnd.oci.image.manifest.v1+json",
        digest:"sha256:8f31eb764fea23b4491b4aa08566949a6be5fde0b3aa7c8b529b0ec0559806a9",
        size:100,
        platform:{os:"linux",architecture:"amd64"}
      },
      {
        mediaType:"application/vnd.oci.image.manifest.v1+json",
        digest:"sha256:14fb0771b1e2333c492fde114920eaa2149f823771d27fd95394651261d8e7d8",
        size:101,
        platform:{os:"linux",architecture:"arm64"}
      },
      {
        mediaType:"application/vnd.oci.image.manifest.v1+json",
        digest:"sha256:1111111111111111111111111111111111111111111111111111111111111111",
        size:50,
        platform:{os:"unknown",architecture:"unknown"},
        annotations:{
          "vnd.docker.reference.type":"attestation-manifest",
          "vnd.docker.reference.digest":"sha256:8f31eb764fea23b4491b4aa08566949a6be5fde0b3aa7c8b529b0ec0559806a9"
        }
      },
      {
        mediaType:"application/vnd.oci.image.manifest.v1+json",
        digest:"sha256:2222222222222222222222222222222222222222222222222222222222222222",
        size:51,
        platform:{os:"unknown",architecture:"unknown"},
        annotations:{
          "vnd.docker.reference.type":"attestation-manifest",
          "vnd.docker.reference.digest":"sha256:14fb0771b1e2333c492fde114920eaa2149f823771d27fd95394651261d8e7d8"
        }
      }
    ]
  }
' > "$index_fixture"

cat > "$fixture_root/scripts/discover-release-merge.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'discover\n' >> "${TEST_LOG:?}"
if [[ "${TEST_MODE:-valid}" == untrusted_release_merge ]]; then
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
mode=${TEST_MODE:-valid}
case "$1" in
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
  expected-release-assets)
    [[ "$2" == v0.3.5 ]]
    for number in $(seq -w 1 35); do printf 'asset-%s\n' "$number"; done
    printf '%s\n' \
      mcp-repl-v0.3.5-container.provenance.sigstore.json \
      mcp-repl-v0.3.5-container.sbom.sigstore.json \
      mcp-repl-v0.3.5-container.spdx.json \
      mcp-repl-v0.3.5-release.json
    ;;
  container-build)
    if [[ "$mode" == bad_build_inputs ]]; then
      printf '{}\n'
    else
      jq -cn '{
        buildkit_image:"docker.io/moby/buildkit:v0.32.2@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        buildx_version:"v0.36.1",
        buildx_sha256:{"linux/amd64":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
      }'
    fi
    ;;
  *) exit 1 ;;
esac
STUB

cat > "$fixture_root/scripts/verify-release-tag.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'tag\n' >> "${TEST_LOG:?}"
[[ "$1" == v0.3.5 ]]
[[ "$2" == 7b51781718975772d96006f167887adb877618e7 ]]
[[ "${TEST_MODE:-valid}" != bad_tag ]]
STUB

cat > "$work/bin/git" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
mode=${TEST_MODE:-valid}
fixture_root=${TEST_FIXTURE_ROOT:?}
release_merge=${TEST_RELEASE_MERGE_SHA:?}
source_sha=${TEST_SOURCE_SHA:?}
case "$*" in
  "-C $fixture_root rev-parse HEAD")
    [[ "$mode" != wrong_checkout ]] || source_sha=${TEST_OTHER_SHA:?}
    printf '%s\n' "$source_sha"
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
        '.github/actionlint.yaml' \
        '.github/workflows/ci.yml' \
        '.github/workflows/release-binaries.yml' \
        '.github/workflows/release-draft-recovery.yml' \
        '.github/workflows/release-latest-recovery.yml' \
        'docs/releases.md' \
        'scripts/publish-container-manifest.sh' \
        'scripts/publish-release.sh' \
        'scripts/smoke-release-latest-recovery.sh' \
        'scripts/test-container-manifest.sh' \
        'scripts/test-publish-release.sh' \
        'scripts/test-release-draft-recovery.sh' \
        'scripts/test-release-latest-recovery.sh' \
        'scripts/test-release-workflow.sh' \
        'scripts/verify-release-draft-recovery.sh' \
        'scripts/verify-release-latest-recovery.sh' \
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

cat > "$work/bin/sha256sum" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
mode=${TEST_MODE:-valid}
path=${1:?}
case "${path##*/}" in
  mcp-repl-v0.3.5-release.json)
    digest=746b2df14a1a6d3cc8779210c3f5dd5e27691853ce231cb7842fd8b704427325
    [[ "$mode" != bad_record_bytes ]] || digest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    ;;
  mcp-repl-v0.3.5-container.provenance.sigstore.json)
    digest=855025d566da00ff9ef19b8d8a6a907f905f9480ceda27fa1a3c4226f5db9211
    [[ "$mode" != bad_provenance_bytes ]] || digest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    ;;
  mcp-repl-v0.3.5-container.sbom.sigstore.json)
    digest=68be24b77e3ccce2ca0a4a8850b75b42fbe3a84a694922e0e85210b2921f592d
    [[ "$mode" != bad_sbom_bundle_bytes ]] || digest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    ;;
  mcp-repl-v0.3.5-container.spdx.json)
    digest=e5cf3b6a397ec1150076700900d183860e143bc54dd9df8b2ea6e63149fcc849
    ;;
  version-index.json | immutable-index.json | final-version-index.json)
    digest=3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c
    [[ "$mode" != smoke_bad_version_digest ]] || digest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    ;;
  latest-index.json | final-latest-index.json)
    digest=3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c
    [[ "$mode" != smoke_bad_latest_digest ]] || digest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    ;;
  v035-index.json)
    digest=3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c
    [[ "$mode" != bad_public_index ]] || digest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    ;;
  *) exec /usr/bin/shasum -a 256 "$path" ;;
esac
printf '%s  %s\n' "$digest" "$path"
STUB

cat > "$work/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
mode=${TEST_MODE:-valid}
repository=${TEST_REPOSITORY:?}
current_run_id=${TEST_CURRENT_RUN_ID:?}
current_run_attempt=${TEST_CURRENT_RUN_ATTEMPT:?}
run_id=${TEST_RUN_ID:?}
run_attempt=${TEST_RUN_ATTEMPT:?}
release_merge=${TEST_RELEASE_MERGE_SHA:?}
release_id=${TEST_RELEASE_ID:?}
record_asset_id=${TEST_RECORD_ASSET_ID:?}
log=${TEST_LOG:?}

success_names=(
  'Verify release merge'
  'Verify package without credentials'
  'Attempt locked crate publication'
  'Verify published crate identity'
  'Publish immutable binary release / Verify immutable release source'
  'Publish immutable binary release / Build and package every target / Validate release target manifest'
  'Publish immutable binary release / Build and package every target / x86_64-unknown-linux-gnu'
  'Publish immutable binary release / Build and package every target / aarch64-unknown-linux-gnu'
  'Publish immutable binary release / Build and package every target / x86_64-unknown-linux-musl'
  'Publish immutable binary release / Build and package every target / aarch64-unknown-linux-musl'
  'Publish immutable binary release / Build and package every target / x86_64-apple-darwin'
  'Publish immutable binary release / Build and package every target / aarch64-apple-darwin'
  'Publish immutable binary release / Build and package every target / x86_64-pc-windows-msvc'
  'Publish immutable binary release / SBOM and attest x86_64-unknown-linux-gnu'
  'Publish immutable binary release / SBOM and attest aarch64-unknown-linux-gnu'
  'Publish immutable binary release / SBOM and attest x86_64-unknown-linux-musl'
  'Publish immutable binary release / SBOM and attest aarch64-unknown-linux-musl'
  'Publish immutable binary release / SBOM and attest x86_64-apple-darwin'
  'Publish immutable binary release / SBOM and attest aarch64-apple-darwin'
  'Publish immutable binary release / SBOM and attest x86_64-pc-windows-msvc'
  'Publish immutable binary release / Stage attested container platforms / Validate container build inputs'
  'Publish immutable binary release / Stage attested container platforms / linux/amd64'
  'Publish immutable binary release / Stage attested container platforms / linux/arm64'
  'Publish immutable binary release / Assemble and attest the container index'
  'Publish immutable binary release / Assemble the canonical release set'
  'Publish immutable binary release / Anonymous prepublication container smoke'
  'Publish immutable binary release / Publish the immutable container version'
  'Publish immutable binary release / Publish the complete immutable GitHub release'
)
failed_name='Publish immutable binary release / Reconcile the mutable latest container alias'
skipped_name='Publish immutable binary release / Anonymous public release smoke'

emit_success_job() {
  local name=$1 step='Complete job' conclusion=success head=$release_merge
  local attempt=$run_attempt workflow='Release publish' status=completed
  case "$name" in
    'Verify published crate identity') step='Reconcile the exact crates.io package' ;;
    'Publish immutable binary release / Assemble the canonical release set')
      step='Preserve the exact canonical release set' ;;
    'Publish immutable binary release / Publish the immutable container version')
      step='Publish or verify the exact version image' ;;
    'Publish immutable binary release / Publish the complete immutable GitHub release')
      step='Create the tag, stage the exact set, and finalize once' ;;
  esac
  if [[ "$mode" == prerequisite_failed && "$name" == 'Verify published crate identity' ]]; then
    conclusion=failure
  fi
  [[ "$mode" != wrong_job_source ]] || head=${TEST_OTHER_SHA:?}
  [[ "$mode" != wrong_job_attempt ]] || attempt=1
  [[ "$mode" != wrong_job_workflow ]] || workflow='Other workflow'
  [[ "$mode" != unfinished_job ]] || status=in_progress
  jq -cn --arg name "$name" --arg step "$step" --arg conclusion "$conclusion" \
    --arg head "$head" --arg workflow "$workflow" --arg status "$status" \
    --argjson run_id "$run_id" --argjson attempt "$attempt" '{
      name:$name,run_id:$run_id,run_attempt:$attempt,head_sha:$head,
      workflow_name:$workflow,status:$status,conclusion:$conclusion,
      steps:[{name:$step,status:"completed",conclusion:$conclusion,number:1}]
    }'
}

emit_failed_job() {
  local login=success reconcile=failure post=success reconcile_name
  reconcile_name="Reconcile latest to GitHub's immutable latest release"
  [[ "$mode" != failed_step_renamed ]] || reconcile_name='Other failure'
  [[ "$mode" != login_failed ]] || login=failure
  [[ "$mode" != reconcile_succeeded ]] || reconcile=success
  [[ "$mode" != post_failed ]] || post=failure
  jq -cn --arg name "$failed_name" --arg head "$release_merge" \
    --arg login "$login" --arg reconcile "$reconcile" --arg post "$post" \
    --arg reconcile_name "$reconcile_name" --argjson run_id "$run_id" \
    --argjson attempt "$run_attempt" '{
      name:$name,run_id:$run_id,run_attempt:$attempt,head_sha:$head,
      workflow_name:"Release publish",status:"completed",conclusion:"failure",
      steps:[
        {name:"Set up job",status:"completed",conclusion:"success",number:1},
        {name:"Run actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",status:"completed",conclusion:"success",number:2},
        {name:"Install checksum-pinned Buildx",status:"completed",conclusion:"success",number:3},
        {name:"Run docker/setup-buildx-action@37fe631027851001ddb9b187196cc803df7f5f0e",status:"completed",conclusion:"success",number:4},
        {name:"Log in only for latest reconciliation",status:"completed",conclusion:$login,number:5},
        {name:$reconcile_name,status:"completed",conclusion:$reconcile,number:6},
        {name:"Post Log in only for latest reconciliation",status:"completed",conclusion:$post,number:10},
        {name:"Post Run docker/setup-buildx-action@37fe631027851001ddb9b187196cc803df7f5f0e",status:"completed",conclusion:"success",number:11},
        {name:"Post Run actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",status:"completed",conclusion:"success",number:12},
        {name:"Complete job",status:"completed",conclusion:"success",number:13}
      ]
    }'
}

release_json() {
  local id=$release_id release_tag=v0.3.5 name=v0.3.5 author='github-actions[bot]'
  local draft=false immutable=true target=main body published='2026-08-23T06:59:06Z'
  case "$mode" in
    wrong_release_id) id=999 ;;
    wrong_release_tag) release_tag=v9.9.9 ;;
    wrong_release_name) name=wrong ;;
    foreign_release) author=octocat ;;
    draft_release) draft=true ;;
    mutable_release) immutable=false ;;
    wrong_release_target) target=develop ;;
    wrong_notes) body=wrong ;;
  esac
  [[ -n "$body" ]] || body=$'### Bug Fixes\n\n- **release:** Accept the Windows SBOM classifier (#237)\n'
  jq -cn --argjson id "$id" --arg tag "$release_tag" --arg name "$name" \
    --arg author "$author" --arg target "$target" --arg body "$body" \
    --arg published "$published" --argjson draft "$draft" \
    --argjson immutable "$immutable" '{
      id:$id,tag_name:$tag,name:$name,target_commitish:$target,body:$body,
      draft:$draft,prerelease:false,immutable:$immutable,published_at:$published,
      author:{login:$author,type:(if $author=="github-actions[bot]" then "Bot" else "User" end)}
    }'
}

emit_asset() {
  local id=$1 name=$2 size=$3 digest=$4 uploader='github-actions[bot]' state=uploaded
  local content_type=application/octet-stream label=null
  [[ "$mode" != foreign_asset ]] || uploader=octocat
  [[ "$mode" != pending_asset ]] || state=new
  [[ "$mode" != wrong_asset_content_type ]] || content_type=text/plain
  [[ "$mode" != labeled_asset ]] || label='"unexpected"'
  jq -cn --argjson id "$id" --arg name "$name" --argjson size "$size" \
    --arg digest "$digest" --arg uploader "$uploader" --arg state "$state" \
    --arg content_type "$content_type" --argjson label "$label" '{
      id:$id,name:$name,size:$size,digest:$digest,state:$state,
      content_type:$content_type,label:$label,
      uploader:{login:$uploader,type:(if $uploader=="github-actions[bot]" then "Bot" else "User" end)}
    }'
}

case "$*" in
  "api repos/$repository/actions/runs/$current_run_id")
    printf 'current-run\n' >> "$log"
    [[ "$mode" != current_run_api_failure ]] || exit 1
    head=${TEST_SOURCE_SHA:?}; path=.github/workflows/release-latest-recovery.yml
    actor=joshrotenberg; event=repository_dispatch; status=in_progress
    [[ "$mode" != wrong_current_run_source ]] || head=${TEST_OTHER_SHA:?}
    [[ "$mode" != wrong_current_run_path ]] || path=.github/workflows/ci.yml
    [[ "$mode" != wrong_current_run_actor ]] || actor=octocat
    [[ "$mode" != wrong_current_run_event ]] || event=push
    jq -cn --arg repository "$repository" --arg head "$head" --arg path "$path" \
      --arg actor "$actor" --arg event "$event" --arg status "$status" \
      --argjson id "$current_run_id" --argjson attempt "$current_run_attempt" '{
        id:$id,name:"Release latest recovery",run_attempt:$attempt,event:$event,
        status:$status,conclusion:null,head_branch:"main",head_sha:$head,path:$path,
        html_url:("https://github.com/"+$repository+"/actions/runs/"+($id|tostring)),
        repository:{full_name:$repository},head_repository:{full_name:$repository},
        actor:{login:$actor,type:"User"},triggering_actor:{login:$actor,type:"User"},
        head_commit:{id:$head}
      }'
    ;;
  "api repos/$repository/actions/runs/$run_id/attempts/$run_attempt")
    printf 'run\n' >> "$log"
    [[ "$mode" != run_api_failure ]] || exit 1
    head=$release_merge; conclusion=failure; path=.github/workflows/release-publish.yml
    id=$run_id; attempt=$run_attempt; actor=joshrotenberg
    [[ "$mode" != wrong_run_source ]] || head=${TEST_OTHER_SHA:?}
    [[ "$mode" != successful_run ]] || conclusion=success
    [[ "$mode" != wrong_run_path ]] || path=.github/workflows/ci.yml
    [[ "$mode" != wrong_run_id ]] || id=1
    [[ "$mode" != wrong_run_attempt ]] || attempt=1
    [[ "$mode" != foreign_run ]] || actor=octocat
    jq -cn --arg repository "$repository" --arg head "$head" \
      --arg conclusion "$conclusion" --arg path "$path" --arg actor "$actor" \
      --argjson id "$id" --argjson run_id "$run_id" --argjson attempt "$attempt" '{
        id:$id,name:"Release publish",run_number:22,workflow_id:340084389,
        run_attempt:$attempt,event:"push",status:"completed",conclusion:$conclusion,
        head_branch:"main",head_sha:$head,path:$path,
        html_url:("https://github.com/"+$repository+"/actions/runs/"+($run_id|tostring)),
        repository:{full_name:$repository},head_repository:{full_name:$repository},
        actor:{login:$actor,type:(if $actor=="joshrotenberg" then "User" else "User" end)},
        triggering_actor:{login:$actor,type:"User"},head_commit:{id:$head}
      }'
    ;;
  "api repos/$repository/actions/runs/$run_id/attempts/$run_attempt/jobs?per_page=100")
    printf 'jobs\n' >> "$log"
    [[ "$mode" != jobs_api_failure ]] || exit 1
    jobs_file=$(mktemp)
    for name in "${success_names[@]}"; do emit_success_job "$name" >> "$jobs_file"; done
    emit_failed_job >> "$jobs_file"
    if [[ "$mode" == public_smoke_has_steps ]]; then
      jq -cn --arg name "$skipped_name" --arg head "$release_merge" \
        --argjson run_id "$run_id" --argjson attempt "$run_attempt" '{
          name:$name,run_id:$run_id,run_attempt:$attempt,head_sha:$head,
          workflow_name:"Release publish",status:"completed",conclusion:"skipped",
          steps:[{name:"unexpected",status:"completed",conclusion:"skipped",number:1}]
        }' >> "$jobs_file"
    else
      jq -cn --arg name "$skipped_name" --arg head "$release_merge" \
        --argjson run_id "$run_id" --argjson attempt "$run_attempt" '{
          name:$name,run_id:$run_id,run_attempt:$attempt,head_sha:$head,
          workflow_name:"Release publish",status:"completed",conclusion:"skipped",steps:[]
        }' >> "$jobs_file"
    fi
    if [[ "$mode" == extra_job ]]; then emit_success_job 'Unexpected job' >> "$jobs_file"; fi
    jq -sc '{total_count:length,jobs:.}' "$jobs_file"
    rm -f "$jobs_file"
    ;;
  "api repos/$repository/releases/$release_id")
    printf 'release\n' >> "$log"
    [[ "$mode" != release_api_failure ]] || exit 1
    release_json
    ;;
  "api repos/$repository/git/ref/tags/v0.3.5")
    printf 'tag-ref\n' >> "$log"
    tag_sha=9a010344d30295cd74c558b1f20877fe719dda39
    tag_type=tag
    if [[ "$mode" == bad_tag_object ]] ||
       { [[ "$mode" == changed_tag_object ]] &&
         [[ $(grep -c '^tag-ref$' "$log") -ge 2 ]]; }; then
      tag_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    fi
    [[ "$mode" != lightweight_tag ]] || tag_type=commit
    jq -cn --arg sha "$tag_sha" --arg type "$tag_type" '{
      ref:"refs/tags/v0.3.5",object:{type:$type,sha:$sha}
    }'
    ;;
  "api repos/$repository/releases/latest")
    printf 'latest\n' >> "$log"
    [[ "$mode" != latest_api_failure ]] || exit 1
    latest_count=$(grep -c '^latest$' "$log")
    if [[ "$mode" == latest_changed && "$latest_count" -ge 2 ]]; then
      mode=wrong_release_id
    elif [[ "$mode" == wrong_latest ]]; then
      mode=wrong_release_tag
    fi
    release_json
    ;;
  "api --paginate --slurp repos/$repository/releases/$release_id/assets?per_page=100")
    printf 'assets\n' >> "$log"
    [[ "$mode" != assets_api_failure ]] || exit 1
    assets_file=$(mktemp)
    for number in $(seq -w 1 35); do
      id=${number#0}; [[ "$id" -gt 0 ]] || id=1
      native_digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
      if [[ "$mode" == wrong_native_tuple && "$number" == 01 ]]; then
        native_digest=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
      fi
      emit_asset "$id" "asset-$number" 1 \
        "$native_digest" \
        >> "$assets_file"
    done
    record_name=mcp-repl-v0.3.5-release.json
    [[ "$mode" != missing_record_asset ]] || record_name=wrong-release.json
    record_id=$record_asset_id
    [[ "$mode" != duplicate_asset_id ]] || record_id=1
    record_asset_digest=sha256:746b2df14a1a6d3cc8779210c3f5dd5e27691853ce231cb7842fd8b704427325
    [[ "$mode" != bad_record_metadata ]] || record_asset_digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    emit_asset "$record_id" "$record_name" 7616 "$record_asset_digest" >> "$assets_file"
    emit_asset 525939528 mcp-repl-v0.3.5-container.provenance.sigstore.json 10996 \
      sha256:855025d566da00ff9ef19b8d8a6a907f905f9480ceda27fa1a3c4226f5db9211 >> "$assets_file"
    emit_asset 525939537 mcp-repl-v0.3.5-container.sbom.sigstore.json 13971 \
      sha256:68be24b77e3ccce2ca0a4a8850b75b42fbe3a84a694922e0e85210b2921f592d >> "$assets_file"
    emit_asset 525939526 mcp-repl-v0.3.5-container.spdx.json 3635 \
      sha256:e5cf3b6a397ec1150076700900d183860e143bc54dd9df8b2ea6e63149fcc849 >> "$assets_file"
    if [[ "$mode" == extra_asset ]]; then
      emit_asset 999 unexpected 1 \
        sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa >> "$assets_file"
    fi
    jq -sc '[.]' "$assets_file"
    rm -f "$assets_file"
    ;;
  "api repos/$repository/releases/assets/$record_asset_id -H Accept: application/octet-stream")
    printf 'record-download\n' >> "$log"
    [[ "$mode" != record_download_failure ]] || exit 1
    cat "${TEST_RECORD_FIXTURE:?}"
    ;;
  "api repos/$repository/releases/assets/525939528 -H Accept: application/octet-stream")
    printf 'provenance-download\n' >> "$log"
    cat "${TEST_PROVENANCE_FIXTURE:?}"
    ;;
  "api repos/$repository/releases/assets/525939537 -H Accept: application/octet-stream")
    printf 'sbom-bundle-download\n' >> "$log"
    cat "${TEST_SBOM_BUNDLE_FIXTURE:?}"
    ;;
  "api repos/$repository/releases/assets/525939526 -H Accept: application/octet-stream")
    printf 'spdx-download\n' >> "$log"
    cat "${TEST_SPDX_FIXTURE:?}"
    ;;
  "attestation trusted-root")
    printf 'trusted-root\n' >> "$log"
    [[ "$mode" != trusted_root_failure ]] || exit 1
    printf '{"trusted":true}\n'
    ;;
  attestation\ verify\ *)
    printf '%s\n' "$*" >> "$log"
    if [[ "$*" == *'oci://'* ||
          "$*" != *'--signer-digest 7b51781718975772d96006f167887adb877618e7'* ]]; then
      echo "attestation verification did not use the local index and signer digest" >&2
      exit 1
    fi
    if [[ "$mode" == provenance_verification_failure &&
          "$*" == *'https://slsa.dev/provenance/v1'* ]] ||
       [[ "$mode" == sbom_verification_failure &&
          "$*" == *'https://spdx.dev/Document/v2.3'* ]]; then
      exit 1
    fi
    ;;
  *)
    echo "unexpected gh call: $*" >&2
    exit 1
    ;;
esac
STUB

cat > "$work/bin/curl" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'curl %s\n' "$*" >> "${TEST_LOG:?}"
mode=${TEST_MODE:-valid}
if [[ "$*" == *'/releases/assets/525939539'* ]]; then
  [[ "$mode" != smoke_record_download_failure ]] || exit 1
  cat "${TEST_RECORD_FIXTURE:?}"
  exit 0
fi
count=$(grep -c '/releases/latest$' "${TEST_LOG:?}")
id=375116865; tag=v0.3.5
if [[ "$mode" == smoke_curl_failure ]]; then exit 1; fi
if [[ "$mode" == smoke_latest_changed && "$count" -ge 2 ]] ||
   [[ "$mode" == smoke_wrong_latest ]]; then
  id=999; tag=v9.9.9
fi
jq -cn --argjson id "$id" --arg tag "$tag" '{
  id:$id,tag_name:$tag,name:$tag,target_commitish:"main",draft:false,
  prerelease:false,immutable:true,published_at:"2026-08-23T06:59:06Z",
  author:{login:"github-actions[bot]",type:"Bot"}
}'
STUB

cat > "$work/bin/docker" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'docker %s\n' "$*" >> "${TEST_LOG:?}"
mode=${TEST_MODE:-valid}
case "$*" in
  "buildx version")
    printf 'github.com/docker/buildx v0.36.1 fixture\n'
    ;;
  "buildx imagetools inspect --raw ghcr.io/joshrotenberg/mcp-repl@sha256:3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c")
    if [[ "$mode" == verifier_acquired_credentials ]]; then
      printf '{"auths":{"ghcr.io":{"auth":"secret"}}}\n' > \
        "${DOCKER_CONFIG:?}/config.json"
    fi
    cat "${TEST_INDEX_FIXTURE:?}"
    ;;
  "buildx imagetools inspect --raw ghcr.io/joshrotenberg/mcp-repl:0.3.5")
    cat "${TEST_INDEX_FIXTURE:?}"
    ;;
  "buildx imagetools inspect --raw ghcr.io/joshrotenberg/mcp-repl:latest")
    latest_inspects=$(grep -c '^docker buildx imagetools inspect --raw ghcr.io/joshrotenberg/mcp-repl:latest$' \
      "${TEST_LOG:?}")
    if [[ "$mode" == smoke_acquired_credentials && "$latest_inspects" -ge 2 ]]; then
      printf '{"auths":{"ghcr.io":{"auth":"secret"}}}\n' > \
        "${DOCKER_CONFIG:?}/config.json"
    fi
    if [[ "$mode" == smoke_bad_mapping ]]; then
      jq -cS '(.manifests[] | select(.platform.architecture == "arm64" and .platform.os == "linux") | .digest) = "sha256:3333333333333333333333333333333333333333333333333333333333333333"' \
        "${TEST_INDEX_FIXTURE:?}"
    elif [[ "$mode" == smoke_tag_moves && "$latest_inspects" -ge 2 ]]; then
      jq -cS '.annotations={changed:"after-runtime"}' "${TEST_INDEX_FIXTURE:?}"
    elif [[ "$mode" == smoke_nonidentical_raw ]]; then
      jq -cS '.annotations={changed:"true"}' "${TEST_INDEX_FIXTURE:?}"
    else
      cat "${TEST_INDEX_FIXTURE:?}"
    fi
    ;;
  pull\ *)
    if [[ "$mode" == smoke_pull_failure ]]; then
      exit 1
    fi
    digest=3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c
    if [[ "$mode" == smoke_pull_wrong_digest &&
          "$*" == *'ghcr.io/joshrotenberg/mcp-repl:latest' ]]; then
      digest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    fi
    printf 'Digest: sha256:%s\nStatus: Downloaded newer image\n' "$digest"
    ;;
  "image inspect --format {{.Id}} "*)
    if [[ "$mode" == smoke_bad_local_id ]]; then
      printf 'not-an-image-id\n'
    else
      printf 'sha256:%s\n' \
        4444444444444444444444444444444444444444444444444444444444444444
    fi
    ;;
  "image inspect --format {{json .RepoDigests}} "*)
    if [[ "$mode" == smoke_wrong_repo_digest ]]; then
      printf '%s\n' \
        '["ghcr.io/joshrotenberg/mcp-repl@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]'
    else
      printf '%s\n' \
        '["ghcr.io/joshrotenberg/mcp-repl@sha256:3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c"]'
    fi
    ;;
  run\ *\ --version)
    [[ "$mode" != smoke_run_failure ]] || exit 1
    if [[ "$mode" == smoke_wrong_version ]]; then
      printf 'mcp-repl 9.9.9\n'
    else
      printf 'mcp-repl 0.3.5\n'
    fi
    ;;
  run\ *--demo*)
    if [[ "$mode" == smoke_bad_demo ]]; then
      printf '{"content":[{"text":"wrong"}]}\n'
    else
      printf '{"content":[{"text":"212.00"}]}\n'
    fi
    ;;
  *) echo "unexpected docker call: $*" >&2; exit 1 ;;
esac
STUB

cat > "$work/bin/timeout" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ $# -ge 2 && "$1" =~ ^[1-9][0-9]*$ ]]
shift
exec "$@"
STUB

chmod +x \
  "$verifier" "$smoke" \
  "$fixture_root/scripts/discover-release-merge.sh" \
  "$fixture_root/scripts/extract-release-notes.sh" \
  "$fixture_root/scripts/release-targets.sh" \
  "$fixture_root/scripts/verify-release-tag.sh" \
  "$work/bin/git" "$work/bin/gh" "$work/bin/sha256sum" \
  "$work/bin/curl" "$work/bin/docker" "$work/bin/timeout"

verification_config="$work/verification-docker"
mkdir -p "$verification_config/cli-plugins"
printf '{}\n' > "$verification_config/config.json"
: > "$verification_config/cli-plugins/docker-buildx"
chmod +x "$verification_config/cli-plugins/docker-buildx"

write_event() {
  local mode=$1 action=release_latest_recovery event_repository=$repository
  local default_branch=main sender=joshrotenberg schema=1 extra=
  local payload_release_id=$release_id payload_release_merge=$release_merge_sha
  local payload_run_id=$run_id payload_attempt=$run_attempt
  case "$mode" in
    wrong_action) action=release_draft_recovery ;;
    wrong_event_repository) event_repository=other/project ;;
    wrong_default_branch) default_branch=develop ;;
    wrong_sender) sender=octocat ;;
    wrong_schema) schema=2 ;;
    wrong_payload_release_id) payload_release_id=999 ;;
    string_payload_release_id) payload_release_id="\"$release_id\"" ;;
    wrong_payload_release_merge) payload_release_merge=$other_sha ;;
    wrong_payload_run_id) payload_run_id=1 ;;
    wrong_payload_attempt) payload_attempt=1 ;;
    extra_payload) extra=',"tag":"v0.3.5"' ;;
  esac
  printf '{"action":"%s","repository":{"full_name":"%s","default_branch":"%s"},"sender":{"login":"%s","type":"User"},"client_payload":{"schema_version":%s,"release_id":%s,"release_merge_sha":"%s","run_id":%s,"run_attempt":%s%s}}\n' \
    "$action" "$event_repository" "$default_branch" "$sender" "$schema" \
    "$payload_release_id" "$payload_release_merge" "$payload_run_id" \
    "$payload_attempt" "$extra" > "$event_file"
}

common_env=(
  GITHUB_ACTOR=joshrotenberg
  GITHUB_EVENT_NAME=repository_dispatch
  GITHUB_EVENT_PATH="$event_file"
  GITHUB_REF=refs/heads/main
  GITHUB_REPOSITORY="$repository"
  GITHUB_SERVER_URL=https://github.com
  GITHUB_SHA="$source_sha"
  GITHUB_TRIGGERING_ACTOR=joshrotenberg
  GITHUB_RUN_ATTEMPT="$current_run_attempt"
  GITHUB_RUN_ID="$current_run_id"
  GITHUB_WORKFLOW="Release latest recovery"
  GITHUB_WORKFLOW_REF="joshrotenberg/mcp-repl/.github/workflows/release-latest-recovery.yml@refs/heads/main"
  GITHUB_WORKFLOW_SHA="$source_sha"
  DOCKER_CONFIG="$verification_config"
  RUNNER_TEMP="$work"
  RUNNER_ARCH=X64
  RUNNER_OS=Linux
  TEST_FIXTURE_ROOT="$fixture_root"
  TEST_CURRENT_RUN_ATTEMPT="$current_run_attempt"
  TEST_CURRENT_RUN_ID="$current_run_id"
  TEST_INDEX_FIXTURE="$index_fixture"
  TEST_LOG="$log"
  TEST_OTHER_SHA="$other_sha"
  TEST_PROVENANCE_FIXTURE="$provenance_fixture"
  TEST_RECORD_ASSET_ID="$record_asset_id"
  TEST_RECORD_FIXTURE="$record_fixture"
  TEST_RELEASE_ID="$release_id"
  TEST_RELEASE_MERGE_SHA="$release_merge_sha"
  TEST_REPOSITORY="$repository"
  TEST_RUN_ATTEMPT="$run_attempt"
  TEST_RUN_ID="$run_id"
  TEST_SBOM_BUNDLE_FIXTURE="$sbom_bundle_fixture"
  TEST_SOURCE_SHA="$source_sha"
  TEST_SPDX_FIXTURE="$spdx_fixture"
)

failures=0
check_verifier() {
  local name=$1 mode=$2 phase=$3 want_status=$4 want_text=$5
  local event_name=${6:-repository_dispatch} event_ref=${7:-refs/heads/main}
  local actor=joshrotenberg controller_attempt=$current_run_attempt
  local runner_arch=X64 output status
  write_event "$mode"
  : > "$output_file"
  : > "$log"
  printf '{}\n' > "$verification_config/config.json"
  [[ "$mode" != wrong_current_actor ]] || actor=octocat
  [[ "$mode" != rerun_controller ]] || controller_attempt=2
  [[ "$mode" != wrong_runner_arch ]] || runner_arch=ARM64
  set +e
  output=$(env PATH="$work/bin:$PATH" TEST_MODE="$mode" \
    "${common_env[@]}" GITHUB_ACTOR="$actor" GITHUB_TRIGGERING_ACTOR="$actor" \
    GITHUB_EVENT_NAME="$event_name" GITHUB_REF="$event_ref" \
    GITHUB_RUN_ATTEMPT="$controller_attempt" \
    RUNNER_ARCH="$runner_arch" \
    "$verifier" "$phase" "$output_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" != "$want_status" || "$output" != *"$want_text"* ]]; then
    printf 'FAIL %s: exit %s wanted %s, missing %q\n%s\n' \
      "$name" "$status" "$want_status" "$want_text" "$output" >&2
    failures=$((failures + 1))
    return
  fi
  if [[ "$want_status" == 0 ]]; then
    expected=$(printf '%s\n' \
      "source_sha=$source_sha" \
      "release_merge_sha=$release_merge_sha" \
      "run_id=$run_id" \
      "run_attempt=$run_attempt" \
      "release_id=$release_id" \
      "release_tag=$tag" \
      "record_asset_id=$record_asset_id" \
      "record_digest=$record_digest" \
      "image=ghcr.io/joshrotenberg/mcp-repl" \
      "source_epoch=1787459270" \
      "container_digest=$manifest_digest" \
      "container_platforms=$platforms" \
      "buildkit_image=$buildkit_image" \
      "buildx_asset=$buildx_asset" \
      "buildx_sha256=$buildx_sha256")
    if [[ $(<"$output_file") != "$expected" ||
          $(grep -c '^latest$' "$log") -ne 2 ||
          $(grep -c '^tag$' "$log") -ne 2 ||
          $(grep -c '^attestation verify ' "$log") -ne 2 ]]; then
      printf 'FAIL %s: trusted output/call topology differs\n%s\n%s\n' \
        "$name" "$(<"$output_file")" "$(<"$log")" >&2
      failures=$((failures + 1))
    fi
  elif [[ -s "$output_file" ]]; then
    printf 'FAIL %s: failed verifier wrote outputs\n' "$name" >&2
    failures=$((failures + 1))
  fi
  printf 'ok   %s\n' "$name"
}

check_verifier "read-only preflight authenticates exact incident" valid preflight 0 "Authenticated exact"
check_verifier "write boundary repeats exact authentication" valid write 0 "Authenticated exact"
check_verifier "only repository dispatch can recover latest" valid preflight 2 "Invalid" push
check_verifier "recovery remains on main" valid preflight 2 "Invalid" repository_dispatch refs/heads/release
check_verifier "recovery rejects reused outputs on a failed-job rerun" rerun_controller preflight 2 "Invalid"
check_verifier "only owner actor can recover" wrong_current_actor preflight 2 "Invalid"
check_verifier "runner architecture is exact" wrong_runner_arch preflight 2 "Invalid"
check_verifier "dispatch action is exact" wrong_action preflight 1 "malformed"
check_verifier "event repository is exact" wrong_event_repository preflight 1 "malformed"
check_verifier "default branch is exact" wrong_default_branch preflight 1 "malformed"
check_verifier "sender is exact" wrong_sender preflight 1 "malformed"
check_verifier "payload schema is exact" wrong_schema preflight 1 "malformed"
check_verifier "release ID value is incident-specific" wrong_payload_release_id preflight 1 "malformed"
check_verifier "release ID type is numeric" string_payload_release_id preflight 1 "malformed"
check_verifier "release merge is incident-specific" wrong_payload_release_merge preflight 1 "malformed"
check_verifier "run ID is incident-specific" wrong_payload_run_id preflight 1 "malformed"
check_verifier "run attempt is incident-specific" wrong_payload_attempt preflight 1 "malformed"
check_verifier "extra payload fields fail closed" extra_payload preflight 1 "malformed"
check_verifier "checkout matches current event source" wrong_checkout preflight 1 "does not match"
check_verifier "release merge must exist" missing_release_merge preflight 1 "strict ancestor"
check_verifier "release merge remains ancestor" unrelated_release_merge preflight 1 "strict ancestor"
check_verifier "product changes force a fresh release" changed_product preflight 1 "outside the reviewed allowlist"
check_verifier "release merge remains trusted" untrusted_release_merge preflight 1 "trusted release merge"
check_verifier "current run API failures are preserved" current_run_api_failure preflight 1 "current Release latest"
check_verifier "current run source is exact" wrong_current_run_source preflight 1 "default-branch latest-recovery"
check_verifier "current run path is exact" wrong_current_run_path preflight 1 "default-branch latest-recovery"
check_verifier "current run actor is exact" wrong_current_run_actor preflight 1 "default-branch latest-recovery"
check_verifier "current run event is exact" wrong_current_run_event preflight 1 "default-branch latest-recovery"
check_verifier "run API failures are preserved" run_api_failure preflight 1 "Could not read"
check_verifier "run source is exact" wrong_run_source preflight 1 "exact failed"
check_verifier "run failure is required" successful_run preflight 1 "exact failed"
check_verifier "run workflow is exact" wrong_run_path preflight 1 "exact failed"
check_verifier "run ID is exact" wrong_run_id preflight 1 "exact failed"
check_verifier "run attempt is exact" wrong_run_attempt preflight 1 "exact failed"
check_verifier "run actors are exact" foreign_run preflight 1 "exact failed"
check_verifier "job API failures are preserved" jobs_api_failure preflight 1 "Could not read jobs"
check_verifier "every job source is exact" wrong_job_source preflight 1 "latest-only topology"
check_verifier "every job attempt is exact" wrong_job_attempt preflight 1 "latest-only topology"
check_verifier "every job workflow is exact" wrong_job_workflow preflight 1 "latest-only topology"
check_verifier "unfinished jobs are rejected" unfinished_job preflight 1 "latest-only topology"
check_verifier "all prerequisites must succeed" prerequisite_failed preflight 1 "latest-only topology"
check_verifier "the reconciliation step alone must fail" failed_step_renamed preflight 1 "latest-only topology"
check_verifier "login failures are not recoverable" login_failed preflight 1 "latest-only topology"
check_verifier "successful reconciliation is not recoverable" reconcile_succeeded preflight 1 "latest-only topology"
check_verifier "post-step failures are not recoverable" post_failed preflight 1 "latest-only topology"
check_verifier "public smoke must have zero skipped steps" public_smoke_has_steps preflight 1 "latest-only topology"
check_verifier "extra jobs are rejected" extra_job preflight 1 "latest-only topology"
check_verifier "release API failures are preserved" release_api_failure preflight 1 "Could not read immutable"
check_verifier "release ID is exact" wrong_release_id preflight 1 "exact immutable"
check_verifier "release tag is exact" wrong_release_tag preflight 1 "exact immutable"
check_verifier "release name is exact" wrong_release_name preflight 1 "exact immutable"
check_verifier "release must be bot-owned" foreign_release preflight 1 "exact immutable"
check_verifier "release must be public" draft_release preflight 1 "exact immutable"
check_verifier "release must be immutable" mutable_release preflight 1 "exact immutable"
check_verifier "release target is main" wrong_release_target preflight 1 "exact immutable"
check_verifier "release notes are byte-exact" wrong_notes preflight 1 "exact immutable"
check_verifier "annotated tag object SHA is exact" bad_tag_object preflight 1 "exact annotated"
check_verifier "lightweight tags are rejected" lightweight_tag preflight 1 "exact annotated"
check_verifier "tag object remains stable" changed_tag_object preflight 1 "changed during authentication"
check_verifier "latest API failures are preserved" latest_api_failure preflight 1 "Could not read GitHub"
check_verifier "GitHub latest must be v0.3.5" wrong_latest preflight 1 "not exact immutable"
check_verifier "GitHub latest remains stable" latest_changed preflight 1 "changed during authentication"
check_verifier "asset API failures are preserved" assets_api_failure preflight 1 "Could not list assets"
check_verifier "no extra assets are accepted" extra_asset preflight 1 "exact trusted 39-asset"
check_verifier "asset IDs are globally unique" duplicate_asset_id preflight 1 "exact trusted 39-asset"
check_verifier "all assets are bot-owned" foreign_asset preflight 1 "exact trusted 39-asset"
check_verifier "all assets are uploaded" pending_asset preflight 1 "exact trusted 39-asset"
check_verifier "asset content types are exact" wrong_asset_content_type preflight 1 "exact trusted 39-asset"
check_verifier "asset labels remain null" labeled_asset preflight 1 "exact trusted 39-asset"
check_verifier "record asset is required" missing_record_asset preflight 1 "exact trusted 39-asset"
check_verifier "record metadata is exact" bad_record_metadata preflight 1 "exact immutable asset identity"
check_verifier "every native asset tuple is record-bound" wrong_native_tuple preflight 1 "canonical 39-file"
check_verifier "record download failures are preserved" record_download_failure preflight 1 "Could not download"
check_verifier "record bytes are exact" bad_record_bytes preflight 1 "differs from immutable"
check_verifier "provenance bytes are exact" bad_provenance_bytes preflight 1 "differs from immutable"
check_verifier "SBOM bundle bytes are exact" bad_sbom_bundle_bytes preflight 1 "differs from immutable"
check_verifier "trusted root is required" trusted_root_failure preflight 1 "trusted root"
check_verifier "provenance verifies against trusted identity" provenance_verification_failure preflight 1 "provenance bundle failed"
check_verifier "SBOM verifies against trusted identity" sbom_verification_failure preflight 1 "SBOM bundle failed"
check_verifier "public immutable index bytes are exact" bad_public_index preflight 1 "Public immutable"
check_verifier "verification remains registry-credential-free" verifier_acquired_credentials preflight 1 "acquired registry credentials"
check_verifier "Buildx inputs remain authenticated" bad_build_inputs preflight 1 "Buildx inputs"
check_verifier "write repeats run authentication" wrong_run_source write 1 "exact failed"
check_verifier "write repeats release authentication" wrong_release_id write 1 "exact immutable"

prepare_smoke_config() {
  local config="$work/anonymous-docker"
  rm -rf "$config"
  mkdir -p "$config/cli-plugins"
  printf '{}\n' > "$config/config.json"
  : > "$config/cli-plugins/docker-buildx"
  chmod +x "$config/cli-plugins/docker-buildx"
  printf '%s\n' "$config"
}

check_smoke() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 tamper=${5:-none}
  local config output status expected_record=$record_digest token=
  write_event "$mode"
  : > "$log"
  config=$(prepare_smoke_config)
  [[ "$tamper" != record ]] || expected_record=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  [[ "$tamper" != token ]] || token=secret
  set +e
  output=$(env PATH="$work/bin:$PATH" TEST_MODE="$mode" \
    "${common_env[@]}" DOCKER_CONFIG="$config" RUNNER_TEMP="$work" \
    GH_TOKEN="$token" \
    EXPECTED_SOURCE_SHA="$source_sha" \
    EXPECTED_RELEASE_MERGE_SHA="$release_merge_sha" \
    EXPECTED_RUN_ID="$run_id" EXPECTED_RUN_ATTEMPT="$run_attempt" \
    EXPECTED_RELEASE_ID="$release_id" EXPECTED_RELEASE_TAG="$tag" \
    EXPECTED_RECORD_ASSET_ID="$record_asset_id" \
    EXPECTED_RECORD_DIGEST="$expected_record" \
    EXPECTED_IMAGE=ghcr.io/joshrotenberg/mcp-repl \
    EXPECTED_SOURCE_EPOCH=1787459270 \
    EXPECTED_CONTAINER_DIGEST="$manifest_digest" \
    EXPECTED_CONTAINER_PLATFORMS="$platforms" \
    "$smoke" 2>&1)
  status=$?
  set -e
  if [[ "$status" != "$want_status" || "$output" != *"$want_text"* ]]; then
    printf 'FAIL %s: exit %s wanted %s, missing %q\n%s\n' \
      "$name" "$status" "$want_status" "$want_text" "$output" >&2
    failures=$((failures + 1))
    return
  fi
  if [[ "$want_status" == 0 ]]; then
    if [[ $(grep -c '^curl ' "$log") -ne 3 ||
          $(grep -c '^docker pull ' "$log") -ne 3 ||
          $(grep -c '^docker run ' "$log") -ne 6 ]] ||
       grep -Eqi '(authorization|login|password)' "$log"; then
      printf 'FAIL %s: anonymous smoke operation topology differs\n%s\n' \
        "$name" "$(<"$log")" >&2
      failures=$((failures + 1))
    fi
  fi
  printf 'ok   %s\n' "$name"
}

check_smoke "public version/latest smoke is anonymous and record-bound" valid 0 "Anonymous public"
check_smoke "preflight output tampering is rejected before smoke" valid 1 "altered before smoke" record
check_smoke "GitHub credentials are rejected from public smoke" valid 2 "Invalid credential-free" token
check_smoke "public latest must be exact v0.3.5" smoke_wrong_latest 1 "not exact immutable"
check_smoke "public latest remains stable through smoke" smoke_latest_changed 1 "changed during anonymous"
check_smoke "public API failures are preserved" smoke_curl_failure 1 "Could not read"
check_smoke "public record download failures are preserved" smoke_record_download_failure 1 "public immutable release record"
check_smoke "version raw digest is record-bound" smoke_bad_version_digest 1 "version index moved"
check_smoke "latest raw digest is record-bound" smoke_bad_latest_digest 1 "latest index moved"
check_smoke "latest platform mapping is record-bound" smoke_bad_mapping 1 "platform mapping differs"
check_smoke "raw version/latest bytes must agree" smoke_nonidentical_raw 1 "not byte-identical"
check_smoke "tag movement during runtime is detected" smoke_tag_moves 1 "moved during runtime"
check_smoke "each pull resolves the immutable digest" smoke_pull_wrong_digest 1 "did not resolve"
check_smoke "local image ID is canonical" smoke_bad_local_id 1 "not locally bound"
check_smoke "local RepoDigests include the release index" smoke_wrong_repo_digest 1 "not locally bound"
check_smoke "anonymous pull failures are preserved" smoke_pull_failure 1 ""
check_smoke "runtime failures are preserved" smoke_run_failure 1 ""
check_smoke "runtime version is exact" smoke_wrong_version 1 "differ"
check_smoke "demo output is exact" smoke_bad_demo 1 ""
check_smoke "public smoke cannot acquire credentials" smoke_acquired_credentials 1 "acquired registry credentials"

if [[ "$failures" -ne 0 ]]; then
  printf '%s release latest recovery tests failed\n' "$failures" >&2
  exit 1
fi
printf 'all release latest recovery tests passed\n'
