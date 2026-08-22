#!/usr/bin/env bash
# Exercise deterministic release archives and their fail-closed input boundary.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
packager="$root/scripts/package-release.sh"
epoch=1700000001
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

fail() {
  echo "release package test failed: $*" >&2
  exit 1
}

for candidate in python3 python; do
  if command -v "$candidate" > /dev/null 2>&1 &&
    "$candidate" -c 'import sys; raise SystemExit(sys.version_info < (3, 8))' \
      > /dev/null 2>&1; then
    test_python=$candidate
    break
  fi
done
[[ -n "${test_python:-}" ]] || fail "Python 3.8 or newer is unavailable"

setup_case() {
  local name=$1
  local binary_name=${2:-mcp-repl}
  case_dir="$work/$name"
  mkdir -p "$case_dir/target/test-target/release"
  cp "$root/README.md" "$root/LICENSE-APACHE" "$root/LICENSE-MIT" "$case_dir/"
  cat > "$case_dir/target/test-target/release/$binary_name" <<'BINARY'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  --version)
    case "${PACKAGE_BINARY_MODE:-valid}" in
      valid | empty_completion | failed_completion | empty_man | failed_man)
        printf '%s\n' 'mcp-repl 1.2.3'
        ;;
      nonzero) printf '%s\n' 'mcp-repl 1.2.3'; exit 9 ;;
      extra_newline) printf 'mcp-repl 1.2.3\n\n' ;;
      trailing_bytes) printf 'mcp-repl 1.2.3\ntrailing' ;;
      wrong) printf '%s\n' 'mcp-repl 9.9.9' ;;
      *) exit 10 ;;
    esac
    ;;
  --completions)
    [[ "${PACKAGE_BINARY_MODE:-valid}" != failed_completion ]] || exit 9
    if [[ "${PACKAGE_BINARY_MODE:-valid}" != empty_completion || "${2:-}" != fish ]]; then
      printf '# %s completion\n' "${2:-missing}"
    fi
    ;;
  --man)
    [[ "${PACKAGE_BINARY_MODE:-valid}" != failed_man ]] || exit 9
    [[ "${PACKAGE_BINARY_MODE:-valid}" == empty_man ]] || printf '%s\n' '.TH MCP-REPL 1'
    ;;
  *) exit 2 ;;
esac
BINARY
  chmod 755 "$case_dir/target/test-target/release/$binary_name"
}

run_package() {
  local mode=$1
  local target=${2:-test-target}
  local version=${3:-v1.2.3}
  local extension=${4:-tar.gz}
  local binary_name=${5:-mcp-repl}
  local binary_version=${6:-1.2.3}
  local package_epoch=${7-$epoch}
  local -a package_environment
  package_environment=("PACKAGE_BINARY_MODE=$mode")
  if [[ $# -ge 8 ]]; then
    package_environment+=("PYTHONPATH=$8")
  fi
  (
    cd "$case_dir"
    env "${package_environment[@]}" "$packager" \
      "$target" \
      "$version" \
      "$extension" \
      "$binary_name" \
      "$binary_version" \
      "$package_epoch"
  ) > "$case_dir/stdout" 2> "$case_dir/stderr"
}

assert_clean_failure() {
  local diagnostic=$1
  local stage_name=${2:-mcp-repl-v1.2.3-test-target}
  grep -Fq "$diagnostic" "$case_dir/stderr" ||
    fail "$(basename "$case_dir") did not report '$diagnostic'"
  [[ ! -e "$case_dir/$stage_name" && ! -L "$case_dir/$stage_name" ]] ||
    fail "$(basename "$case_dir") left a release stage"
  if compgen -G "$case_dir/.mcp-repl-*.package.*" > /dev/null; then
    fail "$(basename "$case_dir") left a temporary package directory"
  fi
  if compgen -G "$case_dir/mcp-repl-*.tar.gz" > /dev/null ||
    compgen -G "$case_dir/mcp-repl-*.zip" > /dev/null ||
    compgen -G "$case_dir/mcp-repl-*.sha256" > /dev/null; then
    fail "$(basename "$case_dir") left a partial release package"
  fi
}

expect_mode_failure() {
  local mode=$1
  local diagnostic=$2
  setup_case "$mode"
  if run_package "$mode"; then
    fail "$mode unexpectedly packaged a binary"
  fi
  assert_clean_failure "$diagnostic"
}

expect_argument_failure() {
  local name=$1
  local diagnostic=$2
  shift 2
  setup_case "$name"
  if run_package valid "$@"; then
    fail "$name unexpectedly accepted unsafe package arguments"
  fi
  assert_clean_failure "$diagnostic"
}

verify_archive() {
  local archive_path=$1
  local extension=$2
  local stage_name=$3
  local binary_name=$4
  local package_epoch=$5
  local case_path=$6
  local minimum_gzip_blocks=${7:-1}
  "$test_python" - \
    "$archive_path" \
    "$extension" \
    "$stage_name" \
    "$binary_name" \
    "$package_epoch" \
    "$case_path" \
    "$minimum_gzip_blocks" <<'PYTHON'
import binascii
import datetime
import gzip
import hashlib
import io
import pathlib
import stat
import struct
import sys
import tarfile
import zipfile


archive = pathlib.Path(sys.argv[1])
extension, stage, binary_name, epoch_arg, case_arg, minimum_blocks_arg = sys.argv[2:]
epoch = int(epoch_arg)
case_path = pathlib.Path(case_arg)
minimum_gzip_blocks = int(minimum_blocks_arg)
expected_names = sorted(
    [
        f"{stage}/",
        f"{stage}/LICENSE-APACHE",
        f"{stage}/LICENSE-MIT",
        f"{stage}/README.md",
        f"{stage}/completions/",
        f"{stage}/completions/mcp-repl.bash",
        f"{stage}/completions/mcp-repl.fish",
        f"{stage}/completions/mcp-repl.zsh",
        f"{stage}/mcp-repl.1",
        f"{stage}/{binary_name}",
    ]
)
directory_names = {f"{stage}/", f"{stage}/completions/"}
expected_payloads = {
    f"{stage}/LICENSE-APACHE": (case_path / "LICENSE-APACHE").read_bytes(),
    f"{stage}/LICENSE-MIT": (case_path / "LICENSE-MIT").read_bytes(),
    f"{stage}/README.md": (case_path / "README.md").read_bytes(),
    f"{stage}/completions/mcp-repl.bash": b"# bash completion\n",
    f"{stage}/completions/mcp-repl.fish": b"# fish completion\n",
    f"{stage}/completions/mcp-repl.zsh": b"# zsh completion\n",
    f"{stage}/mcp-repl.1": b".TH MCP-REPL 1\n",
    f"{stage}/{binary_name}": (
        case_path / "target/test-target/release" / binary_name
    ).read_bytes(),
}

if extension == "tar.gz":
    raw_archive = archive.read_bytes()
    header = raw_archive[:10]
    assert header[:4] == b"\x1f\x8b\x08\x00"
    assert struct.unpack("<I", header[4:8])[0] == epoch
    assert header[8:] == b"\x00\xff"

    position = 10
    blocks = []
    tar_payload = bytearray()
    final = False
    while not final:
        block_header = raw_archive[position]
        position += 1
        assert block_header in (0, 1)
        final = block_header == 1
        length, inverse_length = struct.unpack(
            "<HH", raw_archive[position : position + 4]
        )
        position += 4
        assert inverse_length == (length ^ 0xFFFF)
        block = raw_archive[position : position + length]
        assert len(block) == length
        position += length
        tar_payload.extend(block)
        blocks.append(length)
    assert len(blocks) >= minimum_gzip_blocks
    assert all(length == 65535 for length in blocks[:-1])
    assert 0 <= blocks[-1] <= 65535
    assert position + 8 == len(raw_archive)
    expected_crc, expected_size = struct.unpack("<II", raw_archive[position:])
    assert expected_crc == (binascii.crc32(tar_payload) & 0xFFFFFFFF)
    assert expected_size == (len(tar_payload) & 0xFFFFFFFF)
    assert gzip.decompress(raw_archive) == tar_payload

    with tarfile.open(fileobj=io.BytesIO(tar_payload), mode="r:") as packaged:
        members = packaged.getmembers()
        tar_expected_names = [
            name[:-1] if name.endswith("/") else name for name in expected_names
        ]
        assert [member.name for member in members] == tar_expected_names
        for member in members:
            archive_name = member.name
            normalized_name = archive_name if archive_name.endswith("/") else archive_name + "/"
            is_directory = normalized_name in directory_names
            assert member.isdir() if is_directory else member.isreg()
            assert not member.issym() and not member.islnk()
            assert member.uid == 0 and member.gid == 0
            assert member.uname == "" and member.gname == ""
            assert member.mtime == epoch
            expected_mode = 0o755 if is_directory or archive_name.endswith(binary_name) else 0o644
            assert member.mode == expected_mode
            if not is_directory:
                extracted = packaged.extractfile(member)
                assert extracted is not None
                assert extracted.read() == expected_payloads[archive_name]
elif extension == "zip":
    expected_dos_timestamp = datetime.datetime.fromtimestamp(
        epoch, datetime.timezone.utc
    ).replace(second=datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc).second & ~1)
    timestamp_extra = struct.pack("<HHBi", 0x5455, 5, 1, epoch)
    with zipfile.ZipFile(archive) as packaged:
        assert packaged.comment == b""
        members = packaged.infolist()
        assert [member.filename for member in members] == expected_names
        for member in members:
            is_directory = member.filename in directory_names
            assert member.is_dir() == is_directory
            assert member.create_system == 3
            assert member.date_time == (
                expected_dos_timestamp.year,
                expected_dos_timestamp.month,
                expected_dos_timestamp.day,
                expected_dos_timestamp.hour,
                expected_dos_timestamp.minute,
                expected_dos_timestamp.second,
            )
            assert member.extra == timestamp_extra
            assert member.compress_type == zipfile.ZIP_STORED
            assert member.compress_size == member.file_size
            assert member.create_version == 20 and member.extract_version == 20
            unix_mode = member.external_attr >> 16
            assert stat.S_ISDIR(unix_mode) if is_directory else stat.S_ISREG(unix_mode)
            expected_mode = 0o755 if is_directory or member.filename.endswith(binary_name) else 0o644
            assert stat.S_IMODE(unix_mode) == expected_mode
            if not is_directory:
                payload = packaged.read(member)
                assert payload == expected_payloads[member.filename]
                assert member.CRC == (binascii.crc32(payload) & 0xFFFFFFFF)
else:
    raise AssertionError(extension)

checksum_path = pathlib.Path(f"{archive}.sha256")
expected_checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
assert checksum_path.read_text(encoding="ascii") == f"{expected_checksum}  {archive.name}\n"
PYTHON
}

assert_success() {
  local extension=$1
  local binary_name=$2
  local expected_archive="mcp-repl-v1.2.3-test-target.$extension"
  local archive
  archive=$(<"$case_dir/stdout")
  [[ "$archive" == "$expected_archive" ]] ||
    fail "valid $extension package emitted an unexpected archive name: $archive"
  [[ -f "$case_dir/$archive" && ! -L "$case_dir/$archive" ]] ||
    fail "valid $extension package did not produce a regular archive"
  [[ -f "$case_dir/$archive.sha256" && ! -L "$case_dir/$archive.sha256" ]] ||
    fail "valid $extension package did not produce a regular checksum"
  [[ ! -e "$case_dir/mcp-repl-v1.2.3-test-target" ]] ||
    fail "valid $extension package left its staging directory"
  verify_archive \
    "$case_dir/$archive" \
    "$extension" \
    mcp-repl-v1.2.3-test-target \
    "$binary_name" \
    "$epoch" \
    "$case_dir" || fail "valid $extension archive metadata or payload was not canonical"
}

setup_case valid_tar
run_package valid
assert_success tar.gz mcp-repl

setup_case valid_multiblock_tar
awk 'BEGIN { for (i = 0; i < 70000; i++) printf "x" }' >> "$case_dir/README.md"
run_package valid
assert_success tar.gz mcp-repl
verify_archive \
  "$case_dir/mcp-repl-v1.2.3-test-target.tar.gz" \
  tar.gz \
  mcp-repl-v1.2.3-test-target \
  mcp-repl \
  "$epoch" \
  "$case_dir" \
  2 || fail "tar.gz did not use canonical 65535-byte stored DEFLATE blocks"

setup_case valid_zip mcp-repl.exe
run_package valid test-target v1.2.3 zip mcp-repl.exe
assert_success zip mcp-repl.exe

setup_case valid_ci
run_package valid test-target ci-123-4
[[ $(<"$case_dir/stdout") == mcp-repl-ci-123-4-test-target.tar.gz ]] ||
  fail "a canonical CI rehearsal version produced the wrong archive name"

expect_mode_failure nonzero 'could not report its version'
expect_mode_failure extra_newline 'does not report mcp-repl 1.2.3'
expect_mode_failure trailing_bytes 'does not report mcp-repl 1.2.3'
expect_mode_failure wrong 'does not report mcp-repl 1.2.3'
expect_mode_failure empty_completion 'generated an unsafe or empty fish completion'
expect_mode_failure failed_completion 'could not generate the bash completion'
expect_mode_failure empty_man 'generated an unsafe or empty manual page'
expect_mode_failure failed_man 'could not generate the manual page'

setup_case missing_epoch
if (
  cd "$case_dir"
  "$packager" test-target v1.2.3 tar.gz mcp-repl 1.2.3
) > "$case_dir/stdout" 2> "$case_dir/stderr"; then
  fail "missing epoch unexpectedly packaged a binary"
fi
assert_clean_failure 'usage:'

expect_argument_failure unsafe_target 'unsafe release package identity' \
  '../test-target'
expect_argument_failure long_target 'unsafe release package identity' \
  "$(printf 't%.0s' {1..65})"
expect_argument_failure traversal_version 'unsafe release archive version' \
  test-target '../v1.2.3'
expect_argument_failure leading_zero_version 'unsafe release archive version' \
  test-target v01.2.3
expect_argument_failure incomplete_version 'unsafe release archive version' \
  test-target v1.2
expect_argument_failure long_version 'unsafe release archive version' \
  test-target "v$(printf '9%.0s' {1..65}).1.1"
expect_argument_failure leading_zero_ci_version 'unsafe release archive version' \
  test-target ci-01-1
expect_argument_failure mismatched_version 'release archive and binary versions disagree' \
  test-target v1.2.4
expect_argument_failure unsafe_binary_version 'unsafe release package identity' \
  test-target v1.2.3 tar.gz mcp-repl 01.2.3
expect_argument_failure wrong_shape 'unsupported release package shape' \
  test-target v1.2.3 zip mcp-repl
expect_argument_failure empty_epoch 'source date epoch must be canonical' \
  test-target v1.2.3 tar.gz mcp-repl 1.2.3 ''
expect_argument_failure signed_epoch 'source date epoch must be canonical' \
  test-target v1.2.3 tar.gz mcp-repl 1.2.3 +1700000001
expect_argument_failure leading_zero_epoch 'source date epoch must be canonical' \
  test-target v1.2.3 tar.gz mcp-repl 1.2.3 01700000001
expect_argument_failure old_epoch 'source date epoch must be canonical' \
  test-target v1.2.3 tar.gz mcp-repl 1.2.3 315532799
expect_argument_failure future_epoch 'source date epoch must be canonical' \
  test-target v1.2.3 tar.gz mcp-repl 1.2.3 2147483648
expect_argument_failure text_epoch 'source date epoch must be canonical' \
  test-target v1.2.3 tar.gz mcp-repl 1.2.3 tomorrow

setup_case missing_payload
rm "$case_dir/LICENSE-MIT"
if run_package valid; then
  fail "missing payload source unexpectedly packaged"
fi
assert_clean_failure 'release payload source is missing, linked, or empty: LICENSE-MIT'

setup_case empty_payload
: > "$case_dir/README.md"
if run_package valid; then
  fail "empty payload source unexpectedly packaged"
fi
assert_clean_failure 'release payload source is missing, linked, or empty: README.md'

setup_case linked_payload
rm "$case_dir/README.md"
ln -s LICENSE-MIT "$case_dir/README.md"
if run_package valid; then
  fail "linked payload source unexpectedly packaged"
fi
assert_clean_failure 'release payload source is missing, linked, or empty: README.md'

setup_case linked_binary
mv \
  "$case_dir/target/test-target/release/mcp-repl" \
  "$case_dir/target/test-target/release/real-mcp-repl"
ln -s real-mcp-repl "$case_dir/target/test-target/release/mcp-repl"
if run_package valid; then
  fail "linked binary unexpectedly packaged"
fi
assert_clean_failure 'release binary is missing, linked, or not executable'

expect_stale_output() {
  local name=$1
  local output_name=$2
  local kind=$3
  setup_case "$name"
  outside="$case_dir/outside-output"
  case "$kind" in
    directory) mkdir "$case_dir/$output_name" ;;
    file) printf '%s\n' sentinel > "$case_dir/$output_name" ;;
    symlink) ln -s "$outside" "$case_dir/$output_name" ;;
    *) fail "unknown stale output kind: $kind" ;;
  esac
  if run_package valid; then
    fail "$name overwrote a stale package output"
  fi
  grep -Fq 'release package output already exists' "$case_dir/stderr" ||
    fail "$name did not reject the existing output path"
  if [[ "$kind" == file ]]; then
    [[ "$(<"$case_dir/$output_name")" == sentinel ]] ||
      fail "$name changed the stale output"
  fi
  [[ ! -e "$outside" ]] || fail "$name wrote through a dangling output symlink"
}

expect_stale_output stale_stage mcp-repl-v1.2.3-test-target directory
expect_stale_output stale_archive mcp-repl-v1.2.3-test-target.tar.gz file
expect_stale_output stale_checksum mcp-repl-v1.2.3-test-target.tar.gz.sha256 file
expect_stale_output dangling_stage mcp-repl-v1.2.3-test-target symlink
expect_stale_output dangling_archive mcp-repl-v1.2.3-test-target.tar.gz symlink
expect_stale_output dangling_checksum mcp-repl-v1.2.3-test-target.tar.gz.sha256 symlink

assert_deterministic_format() {
  local extension=$1
  local binary_name=$2
  local suffix=$3
  local archive="mcp-repl-v1.2.3-test-target.$extension"

  setup_case "deterministic_${suffix}_one" "$binary_name"
  first_dir=$case_dir
  touch -t 202001010101 \
    "$case_dir/README.md" \
    "$case_dir/LICENSE-APACHE" \
    "$case_dir/LICENSE-MIT" \
    "$case_dir/target/test-target/release/$binary_name"
  run_package valid test-target v1.2.3 "$extension" "$binary_name"

  setup_case "deterministic_${suffix}_two" "$binary_name"
  second_dir=$case_dir
  touch -t 202512312359 \
    "$case_dir/README.md" \
    "$case_dir/LICENSE-APACHE" \
    "$case_dir/LICENSE-MIT" \
    "$case_dir/target/test-target/release/$binary_name"
  run_package valid test-target v1.2.3 "$extension" "$binary_name"

  cmp -s "$first_dir/$archive" "$second_dir/$archive" ||
    fail "$extension archives differ for the same payload and source epoch"
  cmp -s "$first_dir/$archive.sha256" "$second_dir/$archive.sha256" ||
    fail "$extension checksums differ for the same payload and source epoch"

  setup_case "deterministic_${suffix}_other_epoch" "$binary_name"
  third_dir=$case_dir
  run_package \
    valid \
    test-target \
    v1.2.3 \
    "$extension" \
    "$binary_name" \
    1.2.3 \
    "$((epoch + 2))"
  if cmp -s "$first_dir/$archive" "$third_dir/$archive"; then
    fail "$extension archive ignored the explicit source epoch"
  fi
}

assert_deterministic_format tar.gz mcp-repl tar
assert_deterministic_format zip mcp-repl.exe zip

assert_without_zlib() {
  local extension=$1
  local binary_name=$2
  local suffix=$3
  local reference_dir=$4
  local archive="mcp-repl-v1.2.3-test-target.$extension"

  setup_case "without_zlib_$suffix" "$binary_name"
  no_zlib_path="$case_dir/no-zlib"
  mkdir "$no_zlib_path"
  printf '%s\n' 'raise ImportError("zlib intentionally unavailable")' \
    > "$no_zlib_path/zlib.py"
  run_package \
    valid \
    test-target \
    v1.2.3 \
    "$extension" \
    "$binary_name" \
    1.2.3 \
    "$epoch" \
    "$no_zlib_path"
  assert_success "$extension" "$binary_name"
  cmp -s "$reference_dir/$archive" "$case_dir/$archive" ||
    fail "$extension bytes changed when zlib was unavailable"
}

assert_without_zlib \
  tar.gz \
  mcp-repl \
  tar \
  "$work/deterministic_tar_one"
assert_without_zlib \
  zip \
  mcp-repl.exe \
  zip \
  "$work/deterministic_zip_one"

echo "release package behavior tests passed"
