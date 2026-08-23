#!/usr/bin/env bash
# Credential-free public verification for the one-shot v0.3.5 latest recovery.
set -euo pipefail

if [[ $# -ne 0 ]]; then
  echo "usage: $0" >&2
  exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd)
repository=${GITHUB_REPOSITORY:-}
source_sha=${GITHUB_SHA:-}
event_path=${GITHUB_EVENT_PATH:-}
expected_release_id=375116865
expected_release_merge_sha=7b51781718975772d96006f167887adb877618e7
expected_run_id=32617933653
expected_run_attempt=2
expected_release_tag=v0.3.5
expected_record_asset_id=525939539
expected_record_digest=sha256:746b2df14a1a6d3cc8779210c3f5dd5e27691853ce231cb7842fd8b704427325
expected_source_epoch=1787459270
expected_image=ghcr.io/joshrotenberg/mcp-repl
expected_manifest_digest=sha256:3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c
expected_platforms='[{"platform":"linux/amd64","runnable_digest":"sha256:8f31eb764fea23b4491b4aa08566949a6be5fde0b3aa7c8b529b0ec0559806a9"},{"platform":"linux/arm64","runnable_digest":"sha256:14fb0771b1e2333c492fde114920eaa2149f823771d27fd95394651261d8e7d8"}]'

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
      "${RUNNER_OS:-}" != Linux || "${RUNNER_ARCH:-}" != X64 ||
      ! -f "$event_path" || -L "$event_path" ||
      -n "${GH_TOKEN:-}" || -n "${DOCKER_AUTH_CONFIG:-}" ||
      -n "${REGISTRY_AUTH_FILE:-}" ]]; then
  echo "Invalid credential-free release-latest smoke environment" >&2
  exit 2
fi

for required_command in awk cat cmp cp curl docker git jq mktemp sha256sum timeout tr wc; do
  command -v "$required_command" > /dev/null 2>&1 || {
    echo "release-latest public smoke requires $required_command" >&2
    exit 2
  }
done

if [[ $(git -C "$root" rev-parse HEAD) != "$source_sha" ]]; then
  echo "Public-smoke checkout does not match the recovery event source" >&2
  exit 1
fi

if ! jq -e \
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
      "release_id", "release_merge_sha", "run_attempt", "run_id",
      "schema_version"
    ]) |
    select($payload == {
      schema_version:1,
      release_id:$release_id,
      release_merge_sha:$release_merge_sha,
      run_id:$run_id,
      run_attempt:$run_attempt
    })
  ' "$event_path" > /dev/null; then
  echo "Public smoke request is not the exact v0.3.5 recovery" >&2
  exit 1
fi

if [[ "${EXPECTED_SOURCE_SHA:-}" != "$source_sha" ||
      "${EXPECTED_RELEASE_MERGE_SHA:-}" != "$expected_release_merge_sha" ||
      "${EXPECTED_RUN_ID:-}" != "$expected_run_id" ||
      "${EXPECTED_RUN_ATTEMPT:-}" != "$expected_run_attempt" ||
      "${EXPECTED_RELEASE_ID:-}" != "$expected_release_id" ||
      "${EXPECTED_RELEASE_TAG:-}" != "$expected_release_tag" ||
      "${EXPECTED_RECORD_ASSET_ID:-}" != "$expected_record_asset_id" ||
      "${EXPECTED_RECORD_DIGEST:-}" != "$expected_record_digest" ||
      "${EXPECTED_IMAGE:-}" != "$expected_image" ||
      "${EXPECTED_SOURCE_EPOCH:-}" != "$expected_source_epoch" ||
      "${EXPECTED_CONTAINER_DIGEST:-}" != "$expected_manifest_digest" ||
      "${EXPECTED_CONTAINER_PLATFORMS:-}" != "$expected_platforms" ]]; then
  echo "Authenticated immutable release-record outputs were altered before smoke" >&2
  exit 1
fi

if [[ "${DOCKER_CONFIG:-}" != "${RUNNER_TEMP:-}/anonymous-docker" ||
      ! -d "$DOCKER_CONFIG" || -L "$DOCKER_CONFIG" ||
      ! -f "$DOCKER_CONFIG/config.json" || -L "$DOCKER_CONFIG/config.json" ||
      ! -f "$DOCKER_CONFIG/cli-plugins/docker-buildx" ||
      -L "$DOCKER_CONFIG/cli-plugins/docker-buildx" ||
      ! -x "$DOCKER_CONFIG/cli-plugins/docker-buildx" ||
      $(jq -cS . "$DOCKER_CONFIG/config.json") != '{}' ]]; then
  echo "Public smoke Docker configuration is not fresh and credential-free" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$DOCKER_CONFIG/config.json" "$work/initial-docker-config.json"

public_record="$work/mcp-repl-v0.3.5-release.json"
if ! curl --fail --silent --show-error --location \
  --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --connect-timeout 10 --max-time 60 \
  -H 'Accept: application/octet-stream' \
  -H 'X-GitHub-Api-Version: 2022-11-28' \
  "https://api.github.com/repos/${repository}/releases/assets/${expected_record_asset_id}" \
  > "$public_record" ||
   [[ ! -f "$public_record" || -L "$public_record" || ! -s "$public_record" ]] ||
   [[ $(wc -c < "$public_record" | tr -d '[:space:]') != 7616 ]] ||
   [[ $(sha256sum "$public_record" | awk '{print "sha256:" $1}') != \
      "$expected_record_digest" ]] ||
   ! public_record_identity=$(jq -cer \
     --arg tag "$expected_release_tag" \
     --arg source_sha "$expected_release_merge_sha" \
     --arg image "$expected_image" \
     --arg digest "$expected_manifest_digest" \
     --argjson source_epoch "$expected_source_epoch" \
     --argjson platforms "$expected_platforms" '
       select(type == "object" and .schema_version == 1) |
       select(.package == "mcp-repl" and .tag == $tag and .version == "0.3.5") |
       select(.source_sha == $source_sha and .source_epoch == $source_epoch) |
       select(.container.image == $image and
         .container.manifest_digest == $digest and
         .container.platforms == $platforms) |
       {tag,source_sha,source_epoch,container:{
         image:.container.image,
         manifest_digest:.container.manifest_digest,
         platforms:.container.platforms
       }}
     ' "$public_record"); then
  echo "Could not authenticate the anonymous public immutable release record" >&2
  exit 1
fi
test -n "$public_record_identity"

public_latest_snapshot() {
  local response normalized
  if ! response=$(curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --connect-timeout 10 --max-time 60 \
    -H 'Accept: application/vnd.github+json' \
    -H 'X-GitHub-Api-Version: 2022-11-28' \
    "https://api.github.com/repos/${repository}/releases/latest"); then
    echo "Could not read GitHub's public latest release" >&2
    return 1
  fi
  if ! normalized=$(jq -cer \
    --arg tag "$expected_release_tag" \
    --argjson release_id "$expected_release_id" '
      select(type == "object") |
      select(.id == $release_id and .tag_name == $tag and .name == $tag) |
      select(.target_commitish == "main") |
      select(.draft == false and .prerelease == false and .immutable == true) |
      select(.author.login == "github-actions[bot]" and .author.type == "Bot") |
      select(.published_at | type == "string" and length > 0) |
      {id, tag: .tag_name, published_at}
    ' <<<"$response"); then
    echo "GitHub's public latest release is not exact immutable $expected_release_tag" >&2
    return 1
  fi
  printf '%s\n' "$normalized"
}

sha256_file() {
  local path=$1 digest
  digest=$(sha256sum "$path" | awk '{print $1}')
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || return 1
  printf 'sha256:%s\n' "$digest"
}

inspect_public_index() {
  local reference=$1 output=$2 label=$3 digest mapping
  if ! docker buildx imagetools inspect --raw "$reference" > "$output" ||
     [[ ! -f "$output" || -L "$output" || ! -s "$output" ]]; then
    echo "Could not inspect raw public $label index" >&2
    return 1
  fi
  digest=$(sha256_file "$output")
  if [[ "$digest" != "$expected_manifest_digest" ]]; then
    echo "Public $label index moved from the immutable release record" >&2
    return 1
  fi
  if ! mapping=$(jq -cer -f "$root/scripts/container-runnable-mapping.jq" \
    "$output") || [[ "$mapping" != "$expected_platforms" ]]; then
    echo "Public $label platform mapping differs from the immutable release record" >&2
    return 1
  fi
}

smoke_reference() {
  local reference=$1 label=$2 output expected demo pull_output pull_digest
  local local_id repo_digests
  output="$work/$label-version-output"
  expected="$work/$label-version-expected"
  demo="$work/$label-demo.ndjson"
  pull_output="$work/$label-pull-output"
  if ! timeout 60 docker pull "$reference" > "$pull_output" 2>&1; then
    cat "$pull_output" >&2
    return 1
  fi
  pull_digest=$(awk '$1 == "Digest:" { print $2 }' "$pull_output")
  if [[ "$pull_digest" != "$expected_manifest_digest" ]]; then
    echo "Anonymous $label pull did not resolve the immutable release digest" >&2
    return 1
  fi
  local_id=$(docker image inspect --format '{{.Id}}' "$reference")
  repo_digests=$(docker image inspect --format '{{json .RepoDigests}}' "$reference")
  if [[ ! "$local_id" =~ ^sha256:[0-9a-f]{64}$ ]] ||
     ! jq -e --arg expected "$expected_image@$expected_manifest_digest" '
       type == "array" and index($expected) != null
     ' <<<"$repo_digests" > /dev/null; then
    echo "Anonymous $label pull is not locally bound to the immutable release digest" >&2
    return 1
  fi
  # Execute the resolved local image ID, never the mutable tag again.
  timeout 30 docker run --rm "$local_id" --version > "$output"
  printf 'mcp-repl 0.3.5\n' > "$expected"
  cmp "$expected" "$output"
  timeout 30 docker run --rm "$local_id" \
    --demo --json -e 'convert value=100 from=celsius to=fahrenheit' > "$demo"
  jq -e -s 'length == 1 and .[0].content[0].text == "212.00"' \
    "$demo" > /dev/null
}

latest_before=$(public_latest_snapshot)
version_ref="$expected_image:${expected_release_tag#v}"
latest_ref="$expected_image:latest"
immutable_ref="$expected_image@$expected_manifest_digest"
version_index="$work/version-index.json"
latest_index="$work/latest-index.json"
immutable_index="$work/immutable-index.json"
inspect_public_index "$version_ref" "$version_index" "version"
inspect_public_index "$latest_ref" "$latest_index" "latest"
inspect_public_index "$immutable_ref" "$immutable_index" "immutable"
if ! cmp -s "$version_index" "$latest_index"; then
  echo "Public version and latest raw indexes are not byte-identical" >&2
  exit 1
fi
cmp "$version_index" "$immutable_index"
smoke_reference "$version_ref" version
smoke_reference "$latest_ref" latest
smoke_reference "$immutable_ref" immutable

# Tags are mutable registry references. Re-read both after executing them and
# require byte-for-byte equality with the pre-execution snapshots so a move
# during `docker pull`/`docker run` cannot pass on stale raw evidence.
final_version_index="$work/final-version-index.json"
final_latest_index="$work/final-latest-index.json"
inspect_public_index "$version_ref" "$final_version_index" "final version"
inspect_public_index "$latest_ref" "$final_latest_index" "final latest"
if ! cmp -s "$version_index" "$final_version_index" ||
   ! cmp -s "$latest_index" "$final_latest_index"; then
  echo "Public version or latest moved during runtime smoke" >&2
  exit 1
fi

if ! latest_after=$(public_latest_snapshot); then
  echo "GitHub's immutable latest release changed during anonymous public smoke" >&2
  exit 1
fi
if [[ "$latest_after" != "$latest_before" ]]; then
  echo "GitHub's immutable latest release changed during anonymous public smoke" >&2
  exit 1
fi
if ! cmp -s "$work/initial-docker-config.json" "$DOCKER_CONFIG/config.json" ||
   [[ $(jq -cS . "$DOCKER_CONFIG/config.json") != '{}' ]]; then
  echo "Public smoke acquired registry credentials" >&2
  exit 1
fi

echo "Anonymous public v0.3.5 and latest smoke matched immutable release record $expected_record_digest"
