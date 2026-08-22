#!/usr/bin/env bash
# Exercise install.sh without network access. Platform and download commands
# are stubbed, while real archives and checksums drive the complete installer.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
installer="$root/install.sh"
installer_shell=${INSTALLER_SHELL:-sh}
manifest="$root/release-targets.json"
"$root/scripts/release-targets.sh" validate
glibc_floor=$(jq -er '.linux.gnu_max_glibc' "$manifest")
if [[ ! "$glibc_floor" =~ ^[0-9]+\.[0-9]+$ ]]; then
  echo "installer test needs a major.minor glibc floor, got: $glibc_floor" >&2
  exit 1
fi
glibc_major=${glibc_floor%%.*}
glibc_minor=${glibc_floor#*.}
if [[ "$glibc_minor" -eq 0 ]]; then
  echo "installer test cannot derive a below-floor minor from: $glibc_floor" >&2
  exit 1
fi
glibc_below="$glibc_major.$((glibc_minor - 1))"
glibc_newer="$glibc_major.$((glibc_minor + 1))"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
stub_bin="$work/bin"
wget_bin="$work/wget-bin"
mkdir -p "$stub_bin" "$wget_bin"

real_mv=$(command -v mv)

cat > "$stub_bin/uname" <<'STUB'
#!/bin/sh
set -eu
case "${1:-}" in
  -s) printf '%s\n' "${TEST_OS:?}" ;;
  -m) printf '%s\n' "${TEST_ARCH:?}" ;;
  *) echo "unexpected uname arguments: $*" >&2; exit 1 ;;
esac
STUB

cat > "$stub_bin/getconf" <<'STUB'
#!/bin/sh
set -eu
printf 'getconf %s\n' "$*" >> "${TEST_CALL_LOG:?}"
[ "${1:-}" = GNU_LIBC_VERSION ] || {
  echo "unexpected getconf arguments: $*" >&2
  exit 1
}
case "${TEST_GETCONF_MODE:-new}" in
  new) printf 'glibc %s\n' "${TEST_GLIBC_FLOOR:?}" ;;
  newer) printf 'glibc %s\n' "${TEST_GLIBC_NEWER:?}" ;;
  old) printf 'glibc %s\n' "${TEST_GLIBC_BELOW:?}" ;;
  prefixed_old) printf 'Ubuntu 24.04; glibc %s\n' "${TEST_GLIBC_BELOW:?}" ;;
  musl) printf '%s\n' 'musl libc 1.2.5' ;;
  malformed) printf '%s\n' 'glibc version unknown' ;;
  unavailable) exit 127 ;;
  *) echo "unexpected getconf mode: ${TEST_GETCONF_MODE:-}" >&2; exit 1 ;;
esac
STUB

cat > "$stub_bin/ldd" <<'STUB'
#!/bin/sh
set -eu
printf 'ldd %s\n' "$*" >> "${TEST_CALL_LOG:?}"
[ "${1:-}" = --version ] || {
  echo "unexpected ldd arguments: $*" >&2
  exit 1
}
case "${TEST_LDD_MODE:-new}" in
  new) printf 'ldd (GNU libc) %s\n' "${TEST_GLIBC_FLOOR:?}" ;;
  newer) printf 'ldd (Ubuntu GLIBC %s-test) %s\n' \
    "${TEST_GLIBC_NEWER:?}" "${TEST_GLIBC_NEWER:?}" ;;
  old) printf 'ldd (GNU libc) %s\n' "${TEST_GLIBC_BELOW:?}" ;;
  musl) printf '%s\n' 'musl libc (x86_64)' 'Version 1.2.5'; exit 1 ;;
  malformed) printf '%s\n' 'unknown dynamic linker' ;;
  unavailable) exit 127 ;;
  *) echo "unexpected ldd mode: ${TEST_LDD_MODE:-}" >&2; exit 1 ;;
esac
STUB

cat > "$stub_bin/curl" <<'STUB'
#!/bin/sh
set -eu
url=
output=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) output=${2:-}; shift 2 ;;
    -w) shift 2 ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done
[ -n "$url" ] || { echo 'curl stub received no URL' >&2; exit 1; }
printf '%s\n' "$url" >> "${TEST_FETCH_LOG:?}"

case "$url" in
  https://github.com/joshrotenberg/mcp-repl/releases/latest)
    printf '%s' "${TEST_LATEST_URL:?}"
    ;;
  https://github.com/joshrotenberg/mcp-repl/releases/download/*)
    name=${url##*/}
    case "${TEST_FETCH_MODE:-ok}:$name" in
      missing_archive:*.tar.gz | missing_checksum:*.sha256) exit 22 ;;
    esac
    [ -n "$output" ] || { echo 'curl stub received no output path' >&2; exit 1; }
    cp "${TEST_ASSETS:?}/$name" "$output"
    ;;
  *) echo "unexpected network URL: $url" >&2; exit 1 ;;
esac
STUB

cat > "$stub_bin/wget" <<'STUB'
#!/bin/sh
set -eu
url=
output=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -qO) output=${2:-}; shift 2 ;;
    -qS | --max-redirect=0) shift ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done
[ -n "$url" ] || { echo 'wget stub received no URL' >&2; exit 1; }
printf '%s\n' "$url" >> "${TEST_FETCH_LOG:?}"

case "$url" in
  https://github.com/joshrotenberg/mcp-repl/releases/latest)
    [ -z "$output" ] || { echo 'latest lookup unexpectedly had an output file' >&2; exit 1; }
    printf '  HTTP/1.1 302 Found\n  Location: %s\r\n' "${TEST_LATEST_URL:?}" >&2
    exit 8
    ;;
  https://github.com/joshrotenberg/mcp-repl/releases/download/*)
    name=${url##*/}
    case "${TEST_FETCH_MODE:-ok}:$name" in
      missing_archive:*.tar.gz | missing_checksum:*.sha256) exit 8 ;;
    esac
    [ -n "$output" ] || { echo 'wget stub received no output path' >&2; exit 1; }
    cp "${TEST_ASSETS:?}/$name" "$output"
    ;;
  *) echo "unexpected network URL: $url" >&2; exit 1 ;;
esac
STUB

cat > "$stub_bin/mv" <<'STUB'
#!/bin/sh
set -eu
if [ "${TEST_MV_MODE:-ok}" = fail ]; then
  echo 'simulated atomic rename failure' >&2
  exit 1
fi
exec "${TEST_REAL_MV:?}" "$@"
STUB

chmod +x "$stub_bin/uname" "$stub_bin/getconf" "$stub_bin/ldd" \
  "$stub_bin/curl" "$stub_bin/wget" "$stub_bin/mv"

# A closed PATH proves the wget branch without accidentally finding the host's
# curl. Link only the installer's declared prerequisites and the test stubs.
for command_name in sh mkdir tar gzip mktemp rm cp chmod cmp wc sed tr; do
  command_path=$(command -v "$command_name") || {
    echo "installer test needs $command_name" >&2
    exit 1
  }
  ln -s "$command_path" "$wget_bin/$command_name"
done
if command_path=$(command -v sha256sum); then
  ln -s "$command_path" "$wget_bin/sha256sum"
else
  command_path=$(command -v shasum) || {
    echo 'installer test needs sha256sum or shasum' >&2
    exit 1
  }
  ln -s "$command_path" "$wget_bin/shasum"
fi
for command_name in uname getconf ldd wget mv; do
  ln -s "$stub_bin/$command_name" "$wget_bin/$command_name"
done

sha256() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

case_dir=
assets=
install_dir=
stdout=
stderr=
status=0
case_version=v1.2.3
test_os=Linux
test_arch=x86_64
getconf_mode=new
ldd_mode=new
fetch_mode=ok
mv_mode=ok
test_latest_url=https://github.com/joshrotenberg/mcp-repl/releases/tag/v1.2.3
test_client=curl

start_case() {
  local name=$1
  case_dir="$work/cases/$name"
  assets="$case_dir/assets"
  install_dir="$case_dir/install dir"
  stdout="$case_dir/stdout"
  stderr="$case_dir/stderr"
  mkdir -p "$assets" "$install_dir"
  printf '%s\n' old-install > "$install_dir/mcp-repl"
  chmod 755 "$install_dir/mcp-repl"
  : > "$case_dir/calls"
  : > "$case_dir/fetches"
  case_version=v1.2.3
  test_os=Linux
  test_arch=x86_64
  getconf_mode=new
  ldd_mode=new
  fetch_mode=ok
  mv_mode=ok
  test_latest_url=https://github.com/joshrotenberg/mcp-repl/releases/tag/v1.2.3
  test_client=curl
}

seed_archive() {
  local target=$1
  local binary_mode=${2:-good}
  local version=${3:-v1.2.3}
  local archive="mcp-repl-${version}-${target}.tar.gz"
  local stage="mcp-repl-${version}-${target}"
  local build="$case_dir/build"
  rm -rf "$build"
  mkdir -p "$build/$stage"

  case "$binary_mode" in
    good | duplicate)
      cat > "$build/$stage/mcp-repl" <<EOF
#!/bin/sh
[ "\${1:-}" = --version ] || exit 2
printf '%s\\n' 'mcp-repl ${version#v}'
EOF
      ;;
    wrong_version)
      cat > "$build/$stage/mcp-repl" <<'EOF'
#!/bin/sh
[ "${1:-}" = --version ] || exit 2
printf '%s\n' 'mcp-repl 9.9.9'
EOF
      ;;
    does_not_run)
      cat > "$build/$stage/mcp-repl" <<'EOF'
#!/bin/sh
exit 42
EOF
      ;;
    multiline_version)
      cat > "$build/$stage/mcp-repl" <<EOF
#!/bin/sh
[ "\${1:-}" = --version ] || exit 2
printf '%s\\n' 'mcp-repl ${version#v}' extra
EOF
      ;;
    version_without_newline)
      cat > "$build/$stage/mcp-repl" <<EOF
#!/bin/sh
[ "\${1:-}" = --version ] || exit 2
printf '%s' 'mcp-repl ${version#v}'
EOF
      ;;
    version_trailing_bytes)
      cat > "$build/$stage/mcp-repl" <<EOF
#!/bin/sh
[ "\${1:-}" = --version ] || exit 2
printf '%s\n%s' 'mcp-repl ${version#v}' trailing
EOF
      ;;
    symlink)
      rm -f "$build/$stage/mcp-repl"
      ln -s /bin/sh "$build/$stage/mcp-repl"
      ;;
    hardlink)
      cat > "$build/$stage/payload" <<EOF
#!/bin/sh
[ "\${1:-}" = --version ] || exit 2
printf '%s\\n' 'mcp-repl ${version#v}'
EOF
      ln "$build/$stage/payload" "$build/$stage/mcp-repl"
      ;;
    missing)
      printf '%s\n' decoy > "$build/$stage/not-mcp-repl"
      ;;
    *) echo "unknown binary fixture mode: $binary_mode" >&2; exit 1 ;;
  esac
  [ "$binary_mode" = symlink ] || chmod +x "$build/$stage"/*

  (
    cd "$build"
    case "$binary_mode" in
      duplicate)
        tar czf "$assets/$archive" "$stage/mcp-repl" "$stage/mcp-repl"
        ;;
      hardlink)
        # Ordering makes the expected member a hard link to the first entry.
        tar czf "$assets/$archive" "$stage/payload" "$stage/mcp-repl"
        ;;
      *) tar czf "$assets/$archive" "$stage" ;;
    esac
  )
  local digest
  digest=$(sha256 "$assets/$archive")
  printf '%s  %s\n' "$digest" "$archive" > "$assets/$archive.sha256"
}

run_installer() {
  set +e
  (
    if [[ "$test_client" == wget ]]; then
      export PATH="$wget_bin"
    else
      export PATH="$stub_bin:$PATH"
    fi
    export MCP_REPL_INSTALL_DIR="$install_dir"
    export TEST_OS="$test_os"
    export TEST_ARCH="$test_arch"
    export TEST_GETCONF_MODE="$getconf_mode"
    export TEST_LDD_MODE="$ldd_mode"
    export TEST_GLIBC_FLOOR="$glibc_floor"
    export TEST_GLIBC_BELOW="$glibc_below"
    export TEST_GLIBC_NEWER="$glibc_newer"
    export TEST_FETCH_MODE="$fetch_mode"
    export TEST_MV_MODE="$mv_mode"
    export TEST_REAL_MV="$real_mv"
    export TEST_ASSETS="$assets"
    export TEST_CALL_LOG="$case_dir/calls"
    export TEST_FETCH_LOG="$case_dir/fetches"
    export TEST_LATEST_URL="$test_latest_url"
    if [[ "$case_version" == latest ]]; then
      unset MCP_REPL_VERSION
    else
      export MCP_REPL_VERSION="$case_version"
    fi
    "$installer_shell" "$installer"
  ) > "$stdout" 2> "$stderr"
  status=$?
  set -e
}

assert_no_staged_file() {
  local files=("$install_dir"/.mcp-repl.*)
  if [[ -e "${files[0]}" ]]; then
    echo "temporary installer file was not cleaned up: ${files[*]}" >&2
    exit 1
  fi
}

expect_failure() {
  local label=$1
  local message=$2
  if [[ "$status" -eq 0 ]]; then
    echo "$label unexpectedly succeeded" >&2
    cat "$stdout" "$stderr" >&2
    exit 1
  fi
  grep -Fq "$message" "$stderr" || {
    echo "$label did not report: $message" >&2
    cat "$stderr" >&2
    exit 1
  }
  grep -Fq 'Rust 1.90.0 or newer' "$stderr" || {
    echo "$label omitted the Rust fallback" >&2
    exit 1
  }
  grep -Fq 'cargo install --locked mcp-repl' "$stderr" || {
    echo "$label omitted the Cargo fallback" >&2
    exit 1
  }
  [[ $(<"$install_dir/mcp-repl") == old-install ]] || {
    echo "$label replaced the existing installation on failure" >&2
    exit 1
  }
  assert_no_staged_file
}

expect_success_target() {
  local label=$1 os=$2 arch=$3 getconf=$4 ldd=$5 target=$6
  start_case "$label"
  test_os=$os
  test_arch=$arch
  getconf_mode=$getconf
  ldd_mode=$ldd
  seed_archive "$target"
  run_installer
  if [[ "$status" -ne 0 ]]; then
    echo "$label failed" >&2
    cat "$stdout" "$stderr" >&2
    exit 1
  fi
  grep -Fq "mcp-repl-v1.2.3-${target}.tar.gz" "$case_dir/fetches"
  [[ $("$install_dir/mcp-repl" --version) == 'mcp-repl 1.2.3' ]]
  assert_no_staged_file
}

# Every installer target and every uname alias comes from the authoritative
# manifest. This loop is the mechanical mirror check: adding or changing a
# selector there must work through the real installer here.
installer_selectors=()
while IFS= read -r selector; do
  installer_selectors+=("$selector")
done < <(
  jq -er '
    .native[]
    | select(.installer != null) as $row
    | $row.installer.uname_m[]
    | [$row.target, $row.installer.uname_s, ., ($row.libc // "")]
    | @tsv
  ' "$manifest"
)
expected_selector_count=$(jq -er \
  '[.native[] | select(.installer != null) | .installer.uname_m[]] | length' \
  "$manifest")
[[ "${#installer_selectors[@]}" -eq "$expected_selector_count" ]]
[[ "$expected_selector_count" -gt 0 ]]

selector_number=0
for selector in "${installer_selectors[@]}"; do
  IFS=$'\t' read -r target selector_os selector_arch selector_libc <<<"$selector"
  case "$selector_libc" in
    gnu) selector_getconf=new ;;
    musl) selector_getconf=musl ;;
    '') selector_getconf=unavailable ;;
    *) echo "unexpected installer libc in manifest: $selector_libc" >&2; exit 1 ;;
  esac
  selector_number=$((selector_number + 1))
  expect_success_target \
    "manifest_selector_${selector_number}_${target}_${selector_arch}" \
    "$selector_os" "$selector_arch" "$selector_getconf" unavailable "$target"
  if [[ "$selector_os" != Linux && -s "$case_dir/calls" ]]; then
    echo "non-Linux manifest selector unexpectedly probed libc: $selector" >&2
    exit 1
  fi
done

# Linux libc boundary behavior is retained independently of the manifest
# enumeration: use getconf first, consult ldd only for an inconclusive answer,
# and choose musl whenever glibc cannot be proven new enough.
expect_success_target glibc_floor Linux x86_64 new musl x86_64-unknown-linux-gnu
if grep -Fq 'ldd --version' "$case_dir/calls"; then
  echo 'glibc floor unexpectedly consulted ldd after decisive getconf output' >&2
  exit 1
fi
expect_success_target glibc_newer_alias Linux amd64 newer musl x86_64-unknown-linux-gnu
expect_success_target glibc_old Linux aarch64 old new aarch64-unknown-linux-musl
if grep -Fq 'ldd --version' "$case_dir/calls"; then
  echo 'old glibc unexpectedly consulted ldd after decisive getconf output' >&2
  exit 1
fi
expect_success_target prefixed_old_glibc Linux x86_64 prefixed_old new x86_64-unknown-linux-musl
if grep -Fq 'ldd --version' "$case_dir/calls"; then
  echo 'prefixed old glibc unexpectedly consulted ldd after decisive getconf output' >&2
  exit 1
fi
expect_success_target explicit_musl Linux arm64 musl new aarch64-unknown-linux-musl
if grep -Fq 'ldd --version' "$case_dir/calls"; then
  echo 'explicit musl unexpectedly consulted ldd after decisive getconf output' >&2
  exit 1
fi
expect_success_target ldd_fallback_new Linux x86_64 malformed newer x86_64-unknown-linux-gnu
grep -Fq 'ldd --version' "$case_dir/calls"
expect_success_target ldd_fallback_musl Linux x86_64 unavailable musl x86_64-unknown-linux-musl
expect_success_target unknown_libc Linux x86_64 malformed malformed x86_64-unknown-linux-musl

# The latest redirect is subject to the same strict version validation.
start_case latest
case_version=latest
seed_archive x86_64-unknown-linux-gnu
run_installer
[[ "$status" -eq 0 ]]
grep -Fq 'looking up the latest release' "$stdout"

start_case wget_latest
case_version=latest
test_client=wget
seed_archive x86_64-unknown-linux-gnu
run_installer
if [[ "$status" -ne 0 ]]; then
  echo 'wget latest install failed' >&2
  cat "$stdout" "$stderr" >&2
  exit 1
fi
grep -Fq 'https://github.com/joshrotenberg/mcp-repl/releases/latest' "$case_dir/fetches"
grep -Fq 'mcp-repl-v1.2.3-x86_64-unknown-linux-gnu.tar.gz' "$case_dir/fetches"
[[ $("$install_dir/mcp-repl" --version) == 'mcp-repl 1.2.3' ]]

start_case bad_latest
case_version=latest
seed_archive x86_64-unknown-linux-gnu
test_latest_url=https://github.com/joshrotenberg/mcp-repl/releases/tag/v1.2.3-rc.1
run_installer
expect_failure bad_latest 'release version must match vX.Y.Z'

start_case untrusted_latest_origin
case_version=latest
test_latest_url=https://example.invalid/joshrotenberg/mcp-repl/releases/tag/v1.2.3
run_installer
expect_failure untrusted_latest_origin 'latest release did not resolve under https://github.com/joshrotenberg/mcp-repl/releases/tag/'

# Requested versions must be exactly vX.Y.Z before they reach a URL.
for bad_version in 1.2.3 v1.2 v1..3 v1.2. v.2.3 v1.2.3.4 v1.2.3-rc.1 'v1.2.3/../../bad'; do
  label=${bad_version//[^A-Za-z0-9]/_}
  start_case "bad_version_$label"
  case_version=$bad_version
  seed_archive x86_64-unknown-linux-gnu
  run_installer
  expect_failure "bad requested version $bad_version" 'release version must match vX.Y.Z'
  [[ ! -s "$case_dir/fetches" ]]
done

start_case unsupported_os
test_os=FreeBSD
run_installer
expect_failure unsupported_os 'no prebuilt binary for operating system FreeBSD'

start_case unsupported_arch
test_arch=riscv64
run_installer
expect_failure unsupported_arch 'no prebuilt binary for architecture riscv64'

# Download and checksum failures cannot disturb an existing installation.
start_case missing_archive
seed_archive x86_64-unknown-linux-gnu
fetch_mode=missing_archive
run_installer
expect_failure missing_archive 'no such release asset'

start_case missing_checksum
seed_archive x86_64-unknown-linux-gnu
fetch_mode=missing_checksum
run_installer
expect_failure missing_checksum 'no checksum published'

start_case wrong_checksum_name
seed_archive x86_64-unknown-linux-gnu
checksum_file=$(printf '%s\n' "$assets"/*.sha256)
digest=$(awk '{print $1}' "$checksum_file")
printf '%s  %s\n' "$digest" wrong.tar.gz > "$checksum_file"
run_installer
expect_failure wrong_checksum_name 'checksum does not identify'

start_case uppercase_checksum
seed_archive x86_64-unknown-linux-gnu
checksum_file=$(printf '%s\n' "$assets"/*.sha256)
archive_name=${checksum_file##*/}
archive_name=${archive_name%.sha256}
digest=$(awk '{print $1}' "$checksum_file" | tr '[:lower:]' '[:upper:]')
printf '%s  %s\n' "$digest" "$archive_name" > "$checksum_file"
run_installer
expect_failure uppercase_checksum 'not lowercase SHA-256'

start_case noncanonical_checksum_spacing
seed_archive x86_64-unknown-linux-gnu
checksum_file=$(printf '%s\n' "$assets"/*.sha256)
archive_name=${checksum_file##*/}
archive_name=${archive_name%.sha256}
digest=$(awk '{print $1}' "$checksum_file")
printf '%s %s\n' "$digest" "$archive_name" > "$checksum_file"
run_installer
expect_failure noncanonical_checksum_spacing 'checksum does not identify'

start_case extra_checksum_line
seed_archive x86_64-unknown-linux-gnu
checksum_file=$(printf '%s\n' "$assets"/*.sha256)
printf '%s\n' extra >> "$checksum_file"
run_installer
expect_failure extra_checksum_line 'not one canonical line'

start_case checksum_without_newline
seed_archive x86_64-unknown-linux-gnu
checksum_file=$(printf '%s\n' "$assets"/*.sha256)
line=$(<"$checksum_file")
printf '%s' "$line" > "$checksum_file"
run_installer
expect_failure checksum_without_newline 'not one canonical line'

start_case checksum_trailing_bytes
seed_archive x86_64-unknown-linux-gnu
checksum_file=$(printf '%s\n' "$assets"/*.sha256)
printf '%s' trailing >> "$checksum_file"
run_installer
expect_failure checksum_trailing_bytes 'checksum does not identify'

start_case checksum_mismatch
seed_archive x86_64-unknown-linux-gnu
checksum_file=$(printf '%s\n' "$assets"/*.sha256)
archive_name=${checksum_file##*/}
archive_name=${archive_name%.sha256}
printf '%064d  %s\n' 0 "$archive_name" > "$checksum_file"
run_installer
expect_failure checksum_mismatch 'checksum mismatch'

# Extraction and staged execution are also preconditions to the sole rename.
start_case missing_member
seed_archive x86_64-unknown-linux-gnu missing
run_installer
expect_failure missing_member 'does not contain'

start_case symlink_member
seed_archive x86_64-unknown-linux-gnu symlink
run_installer
expect_failure symlink_member 'is not a regular file'

start_case hardlink_member
seed_archive x86_64-unknown-linux-gnu hardlink
run_installer
expect_failure hardlink_member 'is not a regular file'

start_case duplicate_member
seed_archive x86_64-unknown-linux-gnu duplicate
run_installer
expect_failure duplicate_member 'exactly once'

start_case wrong_binary_version
seed_archive x86_64-unknown-linux-gnu wrong_version
run_installer
expect_failure wrong_binary_version 'did not report exactly'

start_case binary_does_not_run
seed_archive x86_64-unknown-linux-gnu does_not_run
run_installer
expect_failure binary_does_not_run 'did not run on this host'

start_case multiline_binary_version
seed_archive x86_64-unknown-linux-gnu multiline_version
run_installer
expect_failure multiline_binary_version 'did not report exactly'

start_case binary_version_without_newline
seed_archive x86_64-unknown-linux-gnu version_without_newline
run_installer
expect_failure binary_version_without_newline 'did not report exactly'

start_case binary_version_trailing_bytes
seed_archive x86_64-unknown-linux-gnu version_trailing_bytes
run_installer
expect_failure binary_version_trailing_bytes 'did not report exactly'

start_case atomic_rename_failure
seed_archive x86_64-unknown-linux-gnu
mv_mode=fail
run_installer
expect_failure atomic_rename_failure 'could not atomically replace'

printf '%s\n' 'installer behavior tests passed'
