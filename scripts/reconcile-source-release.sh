#!/usr/bin/env bash
# Verify the exact registry state after cargo publish.
#
# If an external service fails after accepting the crate, this credential-free
# step makes the partial state recoverable without republishing. GitHub tag and
# release mutation stays deferred until the complete release set is ready.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <source-sha> <release-action-outcome>" >&2
  exit 2
fi

source_sha=$1
release_outcome=$2

if [[ ! "$source_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "Invalid source-release reconciliation environment" >&2
  exit 2
fi
case "$release_outcome" in
  success | failure) ;;
  *)
    echo "Invalid release action outcome: $release_outcome" >&2
    exit 2
    ;;
esac

max_attempts=${SOURCE_RELEASE_MAX_ATTEMPTS:-12}
retry_delay=${SOURCE_RELEASE_RETRY_DELAY_SECONDS:-5}
if [[ ! "$max_attempts" =~ ^[1-9][0-9]*$ ||
      ! "$retry_delay" =~ ^[0-9]+$ ||
      "$max_attempts" -gt 60 || "$retry_delay" -gt 60 ]]; then
  echo "Invalid source-release retry policy" >&2
  exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

checked_out_sha=$(git rev-parse HEAD)
if [[ "$checked_out_sha" != "$source_sha" ]]; then
  echo "Checkout $checked_out_sha does not match release source $source_sha" >&2
  exit 1
fi

metadata=$(cargo metadata --locked --no-deps --format-version 1)
version=$(jq -er '.packages[] | select(.name == "mcp-repl") | .version' <<<"$metadata")
target_dir=$(jq -er '.target_directory' <<<"$metadata")
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ || -z "$target_dir" ]]; then
  echo "Cargo returned invalid release metadata" >&2
  exit 1
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
registry_response="$work/registry.json"

# Build the exact upload artifact without executing package code. Compilation
# already passed on a separate credential-free runner; this archive is the
# resumed-upload identity.
cargo package --locked --no-verify
crate_file="$target_dir/package/mcp-repl-$version.crate"
if [[ ! -f "$crate_file" ]]; then
  echo "cargo package did not produce $crate_file" >&2
  exit 1
fi
vcs_info="$work/.cargo_vcs_info.json"
if ! tar -xOf "$crate_file" \
  "mcp-repl-$version/.cargo_vcs_info.json" > "$vcs_info"; then
  echo "source package does not contain Cargo VCS metadata" >&2
  exit 1
fi
if ! jq -e \
  --arg source_sha "$source_sha" '
    .git.sha1 == $source_sha and .git.dirty == false
  ' "$vcs_info" > /dev/null; then
  echo "source package VCS metadata does not identify clean commit $source_sha" >&2
  exit 1
fi
if command -v sha256sum > /dev/null 2>&1; then
  local_checksum=$(sha256sum "$crate_file" | awk '{print $1}')
else
  local_checksum=$(shasum -a 256 "$crate_file" | awk '{print $1}')
fi
if [[ ! "$local_checksum" =~ ^[0-9a-f]{64}$ ]]; then
  echo "Could not compute the source package checksum" >&2
  exit 1
fi

registry_state=missing
for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  curl_status=0
  http_status=$(curl \
    --silent \
    --show-error \
    --location \
    --connect-timeout 10 \
    --max-time 30 \
    --user-agent "mcp-repl-release/$version" \
    --output "$registry_response" \
    --write-out '%{http_code}' \
    "https://crates.io/api/v1/crates/mcp-repl/$version") || curl_status=$?

  if [[ "$curl_status" -eq 0 && "$http_status" == 200 ]]; then
    registry_state=present
    break
  fi
  if [[ "$curl_status" -ne 0 || "$http_status" != 404 ]]; then
    if (( attempt == max_attempts )); then
      echo "Could not verify mcp-repl $version on crates.io (curl $curl_status, HTTP $http_status)" >&2
      exit 1
    fi
  elif (( attempt == max_attempts )); then
    break
  fi
  sleep "$retry_delay"
done

if [[ "$registry_state" != present ]]; then
  echo "cargo publish finished with $release_outcome but mcp-repl $version is not on crates.io" >&2
  exit 1
fi
if ! registry_checksum=$(jq -er \
  --arg version "$version" '
    select(.version.crate == "mcp-repl") |
    select(.version.num == $version) |
    select(.version.yanked == false) |
    .version.checksum |
    select(type == "string" and test("^[0-9a-f]{64}$"))
  ' "$registry_response"); then
  echo "crates.io returned invalid or yanked metadata for mcp-repl $version" >&2
  exit 1
fi
if [[ "$registry_checksum" != "$local_checksum" ]]; then
  echo "crates.io checksum for mcp-repl $version does not match this release commit" >&2
  exit 1
fi

echo "Verified crates.io mcp-repl $version matches release source $source_sha"

if [[ "$release_outcome" == failure ]]; then
  echo "Recovered the exact registry state after cargo publish reported failure"
fi
