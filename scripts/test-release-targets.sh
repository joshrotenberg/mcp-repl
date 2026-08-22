#!/usr/bin/env bash
# Exercise strict release target schema, matrix, mirror, and Linux ABI guards.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
helper="$root/scripts/release-targets.sh"
verifier="$root/scripts/verify-release-binary.sh"
manifest="$root/release-targets.json"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

fail() {
  echo "release target test failed: $*" >&2
  exit 1
}

expect_fail() {
  local label=$1
  local diagnostic=$2
  local output
  shift 2

  if output=$("$@" 2>&1); then
    fail "$label unexpectedly succeeded"
  fi
  if [[ "$output" != *"$diagnostic"* ]]; then
    fail "$label did not report '$diagnostic'; output: $output"
  fi
}

"$helper" validate

# Production release consumers must query the manifest instead of growing a
# second target table. Tests are fixtures by design, and the helper itself is
# the validated schema boundary. Third-party tool archives may independently
# use one of the same Rust triples without becoming mcp-repl release targets.
consumer_files=(
  "$root"/.github/workflows/*.yml
  "$root"/scripts/*.sh
  "$root/install.sh"
)
while IFS= read -r release_target; do
  matches=$(grep -nH -F -- "$release_target" "${consumer_files[@]}" || true)
  while IFS=: read -r matched_file line_number _matched_line; do
    [[ -n "$matched_file" ]] || continue
    relative_file=${matched_file#"$root/"}
    case "$relative_file" in
      scripts/release-targets.sh | scripts/test-*.sh) continue ;;
    esac

    # cargo-deny and release-plz publish their own target-named archives.
    context_end=$((line_number + 3))
    context=$(sed -n "${line_number},${context_end}p" "$matched_file")
    if [[ "$context" == *cargo-deny-* || "$context" == *release-plz-* ]]; then
      continue
    fi
    fail "$relative_file:$line_number hardcodes release target $release_target"
  done <<<"$matches"
done < <(jq -r '.native[].target' "$manifest")

while IFS= read -r container_platform; do
  matches=$(grep -nH -F -- "$container_platform" "${consumer_files[@]}" || true)
  while IFS=: read -r matched_file line_number _matched_line; do
    [[ -n "$matched_file" ]] || continue
    relative_file=${matched_file#"$root/"}
    case "$relative_file" in
      scripts/release-targets.sh | scripts/test-*.sh) continue ;;
    esac
    fail "$relative_file:$line_number hardcodes container platform $container_platform"
  done <<<"$matches"
done < <(jq -r '.containers[].platform' "$manifest")

release_matrix=$("$helper" release-matrix)
[[ "$release_matrix" != *$'\n'* ]] || fail "release matrix is not compact JSON"
jq -e '
  (.include | length) == 7 and
  all(.include[];
    (keys | sort) ==
      (["target", "runner", "os", "arch", "libc", "archive", "binary"] | sort)) and
  ([.include[] | select(.os == "linux" and .libc == "musl")] | length) == 2
' <<<"$release_matrix" > /dev/null || fail "release matrix is incomplete"

msrv_matrix=$("$helper" msrv-matrix)
[[ "$msrv_matrix" != *$'\n'* ]] || fail "MSRV matrix is not compact JSON"
jq -e '
  (.include | length) == 3 and
  all(.include[];
    (keys | sort) == (["target", "runner", "os"] | sort)) and
  ([.include[].os] | sort) == (["linux", "macos", "windows"] | sort)
' <<<"$msrv_matrix" > /dev/null || fail "MSRV matrix lacks an operating system"

container_matrix=$("$helper" container-matrix)
[[ "$container_matrix" != *$'\n'* ]] || fail "container matrix is not compact JSON"
jq -e '
  (.include | length) == 2 and
  all(.include[]; (keys | sort) == (["platform", "runner"] | sort)) and
  ([.include[].platform] | sort) == (["linux/amd64", "linux/arm64"] | sort)
' <<<"$container_matrix" > /dev/null || fail "container matrix is incomplete"

container_platforms=$("$helper" container-platforms)
container_platform_count=$(wc -l <<<"$container_platforms" | tr -d '[:space:]')
[[ "$container_platform_count" == 2 ]] ||
  fail "container-platforms emitted $container_platform_count platforms instead of two"
container_platform_json=$(jq -Rsc 'split("\n") | map(select(length > 0))' \
  <<<"$container_platforms")
jq -e --argjson actual "$container_platform_json" \
  '[.containers[].platform] == $actual' "$manifest" > /dev/null ||
  fail "container-platforms order or values are wrong"

rows=$("$helper" rows)
row_count=$(wc -l <<<"$rows" | tr -d '[:space:]')
[[ "$row_count" == 7 ]] || fail "rows emitted $row_count targets instead of seven"
awk -F '\t' 'NF != 3 || $1 == "" || $2 == "" || $3 == "" { exit 1 }' \
  <<<"$rows" || fail "rows output is not target/archive/binary TSV"

asset_version=v9.8.7
assets=$("$helper" expected-assets "$asset_version")
asset_count=$(wc -l <<<"$assets" | tr -d '[:space:]')
[[ "$asset_count" == 14 ]] || fail "expected-assets emitted $asset_count names instead of 14"
asset_json=$(jq -Rsc 'split("\n") | map(select(length > 0))' <<<"$assets")
jq -e --arg version "$asset_version" --argjson actual "$asset_json" '
  .package as $package |
  ([.native[] |
    ("\($package)-\($version)-\(.target).\(.archive)") as $archive |
    $archive, "\($archive).sha256"]) == $actual
' "$manifest" > /dev/null || fail "expected-assets order or names are wrong"
expect_fail "unsafe asset version" "unsafe release version" \
  "$helper" expected-assets '../v9.8.7'

bad="$work/malformed.json"
printf '{\n' > "$bad"
expect_fail "malformed JSON" "is not valid JSON" \
  env RELEASE_TARGETS_FILE="$bad" "$helper" validate

bad="$work/unknown-key.json"
jq '.native[0].unexpected = true' "$manifest" > "$bad"
expect_fail "unknown native key" "strict, typed rows" \
  env RELEASE_TARGETS_FILE="$bad" "$helper" validate

bad="$work/unknown-enum.json"
jq '.native[0].archive = "rar"' "$manifest" > "$bad"
expect_fail "unknown native enum" "supported enum values" \
  env RELEASE_TARGETS_FILE="$bad" "$helper" validate

bad="$work/duplicate.json"
jq '.native[1].target = .native[0].target' "$manifest" > "$bad"
expect_fail "duplicate target" "duplicate or do not cover" \
  env RELEASE_TARGETS_FILE="$bad" "$helper" validate

bad="$work/incomplete.json"
jq '.native = .native[:-1]' "$manifest" > "$bad"
expect_fail "incomplete target set" "seven strict" \
  env RELEASE_TARGETS_FILE="$bad" "$helper" validate

bad="$work/unsafe-runner.json"
jq '.native[0].runner = "self-hosted"' "$manifest" > "$bad"
expect_fail "unsafe runner" "architecture-safe allowlist" \
  env RELEASE_TARGETS_FILE="$bad" "$helper" validate

bad="$work/dangling-msrv.json"
jq '.msrv[0].target = "missing-target"' "$manifest" > "$bad"
expect_fail "dangling MSRV target" "reference unique native targets" \
  env RELEASE_TARGETS_FILE="$bad" "$helper" validate

bad="$work/incomplete-containers.json"
jq '.containers = .containers[:-1]' "$manifest" > "$bad"
expect_fail "incomplete container set" "complete fixed" \
  env RELEASE_TARGETS_FILE="$bad" "$helper" validate

mirror_root="$work/mirrors"
mkdir -p "$mirror_root"
cp "$manifest" "$mirror_root/release-targets.json"
cp "$root/Cargo.toml" "$mirror_root/Cargo.toml"
cp "$root/Dockerfile" "$mirror_root/Dockerfile"
cp "$root/install.sh" "$mirror_root/install.sh"
env RELEASE_TARGETS_ROOT="$mirror_root" "$helper" validate

cargo_drift="$work/cargo-drift"
cp -R "$mirror_root" "$cargo_drift"
sed 's/^[[:space:]]*rust-version[[:space:]]*=.*/rust-version = "0.0.0"/' \
  "$cargo_drift/Cargo.toml" > "$cargo_drift/Cargo.toml.new"
mv "$cargo_drift/Cargo.toml.new" "$cargo_drift/Cargo.toml"
expect_fail "Cargo MSRV drift" "Cargo.toml must mirror" \
  env RELEASE_TARGETS_ROOT="$cargo_drift" "$helper" validate

docker_drift="$work/docker-drift"
cp -R "$mirror_root" "$docker_drift"
sed 's/^ARG RUST_VERSION=.*/ARG RUST_VERSION=0.0.0/' \
  "$docker_drift/Dockerfile" > "$docker_drift/Dockerfile.new"
mv "$docker_drift/Dockerfile.new" "$docker_drift/Dockerfile"
expect_fail "Docker MSRV drift" "Dockerfile must mirror" \
  env RELEASE_TARGETS_ROOT="$docker_drift" "$helper" validate

installer_msrv_drift="$work/installer-msrv-drift"
cp -R "$mirror_root" "$installer_msrv_drift"
sed 's/^RUST_MSRV=.*/RUST_MSRV=0.0.0/' \
  "$installer_msrv_drift/install.sh" > "$installer_msrv_drift/install.sh.new"
mv "$installer_msrv_drift/install.sh.new" "$installer_msrv_drift/install.sh"
expect_fail "installer MSRV drift" "install.sh must mirror the manifest MSRV" \
  env RELEASE_TARGETS_ROOT="$installer_msrv_drift" "$helper" validate

installer_glibc_drift="$work/installer-glibc-drift"
cp -R "$mirror_root" "$installer_glibc_drift"
sed 's/^GNU_LIBC_FLOOR=.*/GNU_LIBC_FLOOR=0.0/' \
  "$installer_glibc_drift/install.sh" > "$installer_glibc_drift/install.sh.new"
mv "$installer_glibc_drift/install.sh.new" "$installer_glibc_drift/install.sh"
expect_fail "installer glibc drift" "install.sh must mirror the manifest GNU floor" \
  env RELEASE_TARGETS_ROOT="$installer_glibc_drift" "$helper" validate

mkdir -p "$work/bin"
cat > "$work/bin/readelf" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

mode=${READELF_MODE:-static}
case "${1:-}" in
  -hW)
    machine='Advanced Micro Devices X86-64'
    [[ "$mode" == wrong_machine ]] && machine=AArch64
    printf 'ELF Header:\n  Machine:                           %s\n' "$machine"
    ;;
  -lW)
    echo 'Program Headers:'
    [[ "$mode" == interp ]] && echo '  INTERP         0x0000000000000350'
    :
    ;;
  -dW)
    if [[ "$mode" == needed ]]; then
      echo ' 0x0000000000000001 (NEEDED) Shared library: [libc.so]'
    else
      echo 'There is no dynamic section in this file.'
    fi
    ;;
  --version-info)
    case "$mode" in
      gnu_ok)
        echo '  0x0010:   Name: GLIBC_2.2.5  Flags: none  Version: 3'
        echo '  0x0020:   Name: GLIBC_2.34  Flags: none  Version: 2'
        ;;
      gnu_new)
        echo '  0x0010:   Name: GLIBC_2.35  Flags: none  Version: 2'
        ;;
      gnu_named_abi)
        echo '  0x0010:   Name: GLIBC_2.34  Flags: none  Version: 3'
        echo '  0x0020:   Name: GLIBC_ABI_DT_RELR  Flags: none  Version: 2'
        ;;
      *) echo 'No version information found in this file.' ;;
    esac
    ;;
  *)
    echo "unexpected readelf arguments: $*" >&2
    exit 2
    ;;
esac
STUB
chmod 755 "$work/bin/readelf"
printf '#!/bin/sh\nexit 0\n' > "$work/release-binary"
chmod 755 "$work/release-binary"

PATH="$work/bin:$PATH" READELF_MODE=static \
  "$verifier" x86_64-unknown-linux-musl "$work/release-binary" > /dev/null
expect_fail "musl interpreter" "ELF INTERP is present" \
  env PATH="$work/bin:$PATH" READELF_MODE=interp \
  "$verifier" x86_64-unknown-linux-musl "$work/release-binary"
expect_fail "musl dependency" "ELF NEEDED is present" \
  env PATH="$work/bin:$PATH" READELF_MODE=needed \
  "$verifier" x86_64-unknown-linux-musl "$work/release-binary"
PATH="$work/bin:$PATH" READELF_MODE=gnu_ok \
  "$verifier" x86_64-unknown-linux-gnu "$work/release-binary" > /dev/null
expect_fail "new GNU ABI" "newer than manifest ceiling GLIBC_2.34" \
  env PATH="$work/bin:$PATH" READELF_MODE=gnu_new \
  "$verifier" x86_64-unknown-linux-gnu "$work/release-binary"
expect_fail "named GNU ABI" "unsupported nonnumeric GLIBC requirements: GLIBC_ABI_DT_RELR" \
  env PATH="$work/bin:$PATH" READELF_MODE=gnu_named_abi \
  "$verifier" x86_64-unknown-linux-gnu "$work/release-binary"
expect_fail "missing GNU versions" "declares no numeric GLIBC" \
  env PATH="$work/bin:$PATH" READELF_MODE=no_glibc \
  "$verifier" x86_64-unknown-linux-gnu "$work/release-binary"
expect_fail "wrong ELF machine" "does not match manifest architecture" \
  env PATH="$work/bin:$PATH" READELF_MODE=wrong_machine \
  "$verifier" x86_64-unknown-linux-musl "$work/release-binary"
expect_fail "unknown binary target" "not a unique manifest entry" \
  env PATH="$work/bin:$PATH" READELF_MODE=static \
  "$verifier" unknown-target "$work/release-binary"

echo "release target manifest and ABI tests passed"
