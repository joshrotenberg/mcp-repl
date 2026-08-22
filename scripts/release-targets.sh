#!/usr/bin/env bash
# Validate and query the single machine-readable release platform manifest.
set -euo pipefail

root=${RELEASE_TARGETS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
manifest=${RELEASE_TARGETS_FILE:-"$root/release-targets.json"}

fail() {
  echo "release-targets: $*" >&2
  exit 1
}

usage() {
  cat >&2 <<EOF
usage: $0 <command> [arguments]

commands:
  validate
  release-matrix
  msrv-matrix
  container-matrix
  container-platforms
  rows
  expected-assets <version>
EOF
  exit 2
}

manifest_requires() {
  local filter=$1
  local diagnostic=$2

  if ! jq -e "$filter" "$manifest" > /dev/null; then
    fail "$diagnostic"
  fi
}

validate_mirrors() {
  local cargo_manifest="$root/Cargo.toml"
  local dockerfile="$root/Dockerfile"
  local installer="$root/install.sh"
  local msrv cargo_line cargo_count cargo_value docker_line docker_count
  local docker_value installer_msrv_line installer_msrv_count installer_msrv
  local installer_glibc_line installer_glibc_count installer_glibc glibc_floor

  msrv=$(jq -r '.rust.msrv' "$manifest")

  if [[ ! -f "$cargo_manifest" || -L "$cargo_manifest" ]]; then
    fail "Cargo.toml must be a regular, non-symlinked MSRV mirror"
  fi
  cargo_line=$(awk '
    /^\[package\][[:space:]]*$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^[[:space:]]*rust-version[[:space:]]*=/ { print }
  ' "$cargo_manifest")
  cargo_count=$(printf '%s\n' "$cargo_line" | sed '/^$/d' | wc -l | tr -d '[:space:]')
  cargo_value=$(printf '%s\n' "$cargo_line" | sed -n \
    's/^[[:space:]]*rust-version[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p')
  if [[ "$cargo_count" != 1 || "$cargo_value" != "$msrv" ]]; then
    fail "Cargo.toml must mirror the manifest MSRV exactly as rust-version = \"$msrv\""
  fi

  if [[ ! -f "$dockerfile" || -L "$dockerfile" ]]; then
    fail "Dockerfile must be a regular, non-symlinked MSRV mirror"
  fi
  docker_line=$(sed -n '/^[[:space:]]*ARG[[:space:]][[:space:]]*RUST_VERSION[[:space:]]*=/p' \
    "$dockerfile")
  docker_count=$(printf '%s\n' "$docker_line" | sed '/^$/d' | wc -l | tr -d '[:space:]')
  docker_value=$(printf '%s\n' "$docker_line" | sed -n \
    's/^[[:space:]]*ARG[[:space:]][[:space:]]*RUST_VERSION[[:space:]]*=[[:space:]]*\([^[:space:]#]*\)[[:space:]]*$/\1/p')
  if [[ "$docker_count" != 1 || "$docker_value" != "$msrv" ]]; then
    fail "Dockerfile must mirror the manifest MSRV exactly as ARG RUST_VERSION=$msrv"
  fi

  if [[ ! -f "$installer" || -L "$installer" ]]; then
    fail "install.sh must be a regular, non-symlinked policy mirror"
  fi
  installer_msrv_line=$(sed -n '/^RUST_MSRV=/p' "$installer")
  installer_msrv_count=$(printf '%s\n' "$installer_msrv_line" | sed '/^$/d' |
    wc -l | tr -d '[:space:]')
  installer_msrv=$(printf '%s\n' "$installer_msrv_line" |
    sed -n 's/^RUST_MSRV=\([0-9][0-9.]*\)$/\1/p')
  if [[ "$installer_msrv_count" != 1 || "$installer_msrv" != "$msrv" ]]; then
    fail "install.sh must mirror the manifest MSRV exactly as RUST_MSRV=$msrv"
  fi

  installer_glibc_line=$(sed -n '/^GNU_LIBC_FLOOR=/p' "$installer")
  installer_glibc_count=$(printf '%s\n' "$installer_glibc_line" | sed '/^$/d' |
    wc -l | tr -d '[:space:]')
  installer_glibc=$(printf '%s\n' "$installer_glibc_line" |
    sed -n 's/^GNU_LIBC_FLOOR=\([0-9][0-9.]*\)$/\1/p')
  glibc_floor=$(jq -r '.linux.gnu_max_glibc' "$manifest")
  if [[ "$installer_glibc_count" != 1 ||
        "$installer_glibc" != "$glibc_floor" ]]; then
    fail "install.sh must mirror the manifest GNU floor exactly as GNU_LIBC_FLOOR=$glibc_floor"
  fi
}

validate_manifest() {
  if ! command -v jq > /dev/null 2>&1; then
    fail "jq is required"
  fi
  if [[ ! -f "$manifest" || -L "$manifest" ]]; then
    fail "$manifest must be a regular, non-symlinked file"
  fi
  if ! jq empty "$manifest" > /dev/null 2>&1; then
    fail "$manifest is not valid JSON"
  fi

  manifest_requires '
    type == "object" and
    ((keys | sort) ==
      (["schema_version", "package", "rust", "linux", "native", "msrv", "containers"] | sort))
  ' "manifest has an unknown or missing top-level key"
  manifest_requires '
    .schema_version == 1 and
    .package == "mcp-repl" and
    (.rust | type) == "object" and
    ((.rust | keys | sort) == (["msrv", "rehearsal"] | sort)) and
    (.rust.msrv | type) == "string" and
    (.rust.msrv | test("^[0-9]+\\.[0-9]+\\.[0-9]+$")) and
    .rust.rehearsal == "stable" and
    (.linux | type) == "object" and
    ((.linux | keys | sort) == (["fallback_libc", "gnu_max_glibc"] | sort)) and
    (.linux.gnu_max_glibc | type) == "string" and
    (.linux.gnu_max_glibc | test("^[0-9]+\\.[0-9]+$")) and
    .linux.fallback_libc == "musl"
  ' "manifest metadata, Rust policy, or Linux ABI policy is invalid"
  manifest_requires '
    (.native | type) == "array" and
    (.native | length) == 7 and
    all(.native[];
      type == "object" and
      ((keys | sort) ==
        (["target", "runner", "os", "arch", "libc", "archive", "binary", "installer"] | sort)) and
      (.target | type) == "string" and
      (.target | test("^[A-Za-z0-9_][A-Za-z0-9_.-]*$")) and
      (.runner | type) == "string" and
      (.os == "linux" or .os == "macos" or .os == "windows") and
      (.arch == "x86_64" or .arch == "aarch64") and
      (.libc == null or .libc == "gnu" or .libc == "musl") and
      (.archive == "tar.gz" or .archive == "zip") and
      (.binary == "mcp-repl" or .binary == "mcp-repl.exe"))
  ' "native targets must be seven strict, typed rows with supported enum values"
  manifest_requires '
    ([.native[].target] | length) == ([.native[].target] | unique | length) and
    ([.native[] | [.os, .arch, (.libc // "")]] | sort) ==
      ([
        ["linux", "x86_64", "gnu"],
        ["linux", "x86_64", "musl"],
        ["linux", "aarch64", "gnu"],
        ["linux", "aarch64", "musl"],
        ["macos", "x86_64", ""],
        ["macos", "aarch64", ""],
        ["windows", "x86_64", ""]
      ] | sort)
  ' "native targets are duplicate or do not cover the complete supported platform set"
  manifest_requires '
    all(.native[];
      if .os == "linux" then
        .target == (.arch + "-unknown-linux-" + .libc) and
        .archive == "tar.gz" and .binary == "mcp-repl"
      elif .os == "macos" then
        .target == (.arch + "-apple-darwin") and
        .libc == null and .archive == "tar.gz" and .binary == "mcp-repl"
      else
        .target == "x86_64-pc-windows-msvc" and
        .arch == "x86_64" and .libc == null and
        .archive == "zip" and .binary == "mcp-repl.exe"
      end)
  ' "native target triples, archive formats, or binary names are inconsistent"
  manifest_requires '
    all(.native[];
      if .os == "linux" and .arch == "x86_64" then .runner == "ubuntu-22.04"
      elif .os == "linux" and .arch == "aarch64" then .runner == "ubuntu-22.04-arm"
      elif .os == "macos" and .arch == "x86_64" then .runner == "macos-15-intel"
      elif .os == "macos" and .arch == "aarch64" then .runner == "macos-15"
      elif .os == "windows" and .arch == "x86_64" then .runner == "windows-2025"
      else false
      end)
  ' "native target uses a runner outside the fixed architecture-safe allowlist"
  # $row is a jq variable, not a shell expansion.
  # shellcheck disable=SC2016
  manifest_requires '
    all(.native[];
      . as $row |
      if $row.os == "windows" then $row.installer == null
      else
        ($row.installer | type) == "object" and
        (($row.installer | keys | sort) == (["uname_s", "uname_m"] | sort)) and
        ($row.installer.uname_m | type) == "array" and
        ($row.installer.uname_m | length) == 2 and
        ($row.installer.uname_m | unique | length) == 2 and
        (if $row.os == "linux"
         then $row.installer.uname_s == "Linux"
         else $row.installer.uname_s == "Darwin"
         end) and
        (if $row.arch == "x86_64"
         then $row.installer.uname_m == ["x86_64", "amd64"]
         else $row.installer.uname_m == ["aarch64", "arm64"]
         end)
      end)
  ' "installer uname selectors or architecture aliases are invalid"
  # $manifest and $check are jq variables, not shell expansions.
  # shellcheck disable=SC2016
  manifest_requires '
    . as $manifest |
    ($manifest.msrv | type) == "array" and
    ($manifest.msrv | length) == 3 and
    all($manifest.msrv[];
      type == "object" and
      ((keys | sort) == (["target", "runner"] | sort)) and
      (.target | type) == "string" and (.runner | type) == "string") and
    ([$manifest.msrv[].target] | length) ==
      ([$manifest.msrv[].target] | unique | length) and
    all($manifest.msrv[];
      . as $check |
      ([$manifest.native[] |
        select(.target == $check.target and .runner == $check.runner)] | length) == 1) and
    ([$manifest.msrv[] as $check | $manifest.native[] |
      select(.target == $check.target) | .os] | sort) ==
      (["linux", "macos", "windows"] | sort)
  ' "MSRV rows must reference unique native targets and cover Linux, macOS, and Windows"
  manifest_requires '
    (.containers | type) == "array" and
    (.containers | length) == 2 and
    all(.containers[];
      type == "object" and
      ((keys | sort) == (["platform", "runner"] | sort))) and
    ([.containers[] | [.platform, .runner]] | sort) ==
      ([
        ["linux/amd64", "ubuntu-22.04"],
        ["linux/arm64", "ubuntu-22.04-arm"]
      ] | sort)
  ' "container rows must be the complete fixed native-runner platform set"

  validate_mirrors
}

[[ $# -ge 1 ]] || usage
command=$1
shift

case "$command" in
  validate)
    [[ $# -eq 0 ]] || usage
    validate_manifest
    ;;
  release-matrix)
    [[ $# -eq 0 ]] || usage
    validate_manifest
    jq -c '{include: [.native[] | {
      target, runner, os, arch, libc, archive, binary
    }]}' "$manifest"
    ;;
  msrv-matrix)
    [[ $# -eq 0 ]] || usage
    validate_manifest
    jq -c '
      .native as $native |
      {include: [.msrv[] as $check |
        ($native[] | select(.target == $check.target)) as $target |
        {
          target: $check.target,
          runner: $check.runner,
          os: $target.os
        }]}
    ' "$manifest"
    ;;
  container-matrix)
    [[ $# -eq 0 ]] || usage
    validate_manifest
    jq -c '{include: [.containers[] | {platform, runner}]}' "$manifest"
    ;;
  container-platforms)
    [[ $# -eq 0 ]] || usage
    validate_manifest
    jq -r '.containers[].platform' "$manifest"
    ;;
  rows)
    [[ $# -eq 0 ]] || usage
    validate_manifest
    jq -r '.native[] | [.target, .archive, .binary] | @tsv' "$manifest"
    ;;
  expected-assets)
    [[ $# -eq 1 ]] || usage
    version=$1
    if [[ ! "$version" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]]; then
      fail "unsafe release version: $version"
    fi
    validate_manifest
    jq -r --arg version "$version" '
      .package as $package |
      .native[] |
      ("\($package)-\($version)-\(.target).\(.archive)") as $archive |
      $archive, "\($archive).sha256"
    ' "$manifest"
    ;;
  *)
    usage
    ;;
esac
