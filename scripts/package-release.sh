#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 <target> <archive-version> <archive-extension> <binary-name> <binary-version> <source-date-epoch>" >&2
  exit 2
fi

target=$1
version=$2
extension=$3
binary_name=$4
binary_version=$5
source_date_epoch=$6

semver_component='(0|[1-9][0-9]*)'
semver_pattern="^${semver_component}\\.${semver_component}\\.${semver_component}$"
if [[ ! "$target" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]*$ ||
      ${#target} -gt 64 ||
      ! "$binary_version" =~ $semver_pattern ||
      ${#binary_version} -gt 32 ]]; then
  echo "unsafe release package identity" >&2
  exit 2
fi
if [[ ! "$version" =~ ^v${semver_component}\.${semver_component}\.${semver_component}$ &&
      ! "$version" =~ ^ci-${semver_component}-${semver_component}$ ]] ||
    [[ ${#version} -gt 64 ]]; then
  echo "unsafe release archive version: $version" >&2
  exit 2
fi
if [[ "$version" == v* && "${version#v}" != "$binary_version" ]]; then
  echo "release archive and binary versions disagree: $version/$binary_version" >&2
  exit 2
fi
case "$extension:$binary_name" in
  tar.gz:mcp-repl | zip:mcp-repl.exe) ;;
  *)
    echo "unsupported release package shape: $extension/$binary_name" >&2
    exit 2
    ;;
esac

# ZIP's extended timestamp encodes signed 32-bit Unix seconds. Requiring its
# range and the DOS timestamp floor means neither format needs a silent clamp,
# and every archive member can carry the caller's exact epoch.
if [[ ! "$source_date_epoch" =~ ^(0|[1-9][0-9]{0,9})$ ]] ||
  ((source_date_epoch < 315532800 || source_date_epoch > 2147483647)); then
  echo "source date epoch must be canonical UTC seconds from 1980 through 2038" >&2
  exit 2
fi

archive_python=${RELEASE_PACKAGE_PYTHON:-}
if [[ -n "$archive_python" ]]; then
  if ! command -v "$archive_python" > /dev/null 2>&1 ||
    ! "$archive_python" -c 'import sys; raise SystemExit(sys.version_info < (3, 8))'; then
    echo "RELEASE_PACKAGE_PYTHON must name Python 3.8 or newer" >&2
    exit 1
  fi
else
  for candidate in python3 python; do
    if command -v "$candidate" > /dev/null 2>&1 &&
      "$candidate" -c 'import sys; raise SystemExit(sys.version_info < (3, 8))' \
        > /dev/null 2>&1; then
      archive_python=$candidate
      break
    fi
  done
  if [[ -z "$archive_python" ]]; then
    echo "Python 3.8 or newer is required to create deterministic release archives" >&2
    exit 1
  fi
fi

# The caller receives these fields from the validated release-target manifest.
# Keeping the packager data-driven prevents its own target table from drifting.
binary="target/${target}/release/${binary_name}"
if [[ ! -f "$binary" || -L "$binary" || ! -x "$binary" ]]; then
  echo "release binary is missing, linked, or not executable: $binary" >&2
  exit 1
fi
for source_file in README.md LICENSE-APACHE LICENSE-MIT; do
  if [[ ! -f "$source_file" || -L "$source_file" || ! -s "$source_file" ]]; then
    echo "release payload source is missing, linked, or empty: $source_file" >&2
    exit 1
  fi
done

version_output=$(mktemp)
expected_version_output=$(mktemp)
cleanup_version_output() {
  rm -f "$version_output" "$expected_version_output"
}
trap cleanup_version_output EXIT
if ! "$binary" --version > "$version_output"; then
  echo "release binary could not report its version: $binary" >&2
  exit 1
fi
printf 'mcp-repl %s\n' "$binary_version" > "$expected_version_output"
if ! cmp -s "$version_output" "$expected_version_output"; then
  echo "release binary does not report mcp-repl $binary_version: $binary" >&2
  exit 1
fi
cleanup_version_output
trap - EXIT

stage="mcp-repl-${version}-${target}"
archive="${stage}.${extension}"
for output in "$stage" "$archive" "$archive.sha256"; do
  if [[ -e "$output" || -L "$output" ]]; then
    echo "release package output already exists: $output" >&2
    exit 1
  fi
done

# Build beside the final outputs so each final rename stays on one filesystem.
# A failed package leaves no partial archive or predictable staging directory.
work=$(mktemp -d "./.${stage}.package.XXXXXX")
stage_path="$work/$stage"
archive_path="$work/$archive"
checksum_path="$work/$archive.sha256"
archive_installed=false
checksum_installed=false
cleanup_package() {
  local status=$?
  if [[ $status -ne 0 ]]; then
    if [[ "$checksum_installed" == true ]]; then
      rm -f "$archive.sha256"
    fi
    if [[ "$archive_installed" == true ]]; then
      rm -f "$archive"
    fi
  fi
  rm -rf "$work"
  return "$status"
}
trap cleanup_package EXIT

umask 022
mkdir -p "$stage_path/completions"
cp "$binary" "$stage_path/$binary_name"
cp README.md LICENSE-APACHE LICENSE-MIT "$stage_path/"
chmod 755 "$stage_path" "$stage_path/completions" "$stage_path/$binary_name"
chmod 644 \
  "$stage_path/README.md" \
  "$stage_path/LICENSE-APACHE" \
  "$stage_path/LICENSE-MIT"

# Generate these from the binary being shipped so the documentation cannot
# drift from its actual flags. Every configured runner is target-native.
for shell in bash zsh fish; do
  completion="$stage_path/completions/mcp-repl.$shell"
  if ! "$binary" --completions "$shell" > "$completion"; then
    echo "release binary could not generate the $shell completion" >&2
    exit 1
  fi
  if [[ ! -f "$completion" || -L "$completion" || ! -s "$completion" ]]; then
    echo "release binary generated an unsafe or empty $shell completion" >&2
    exit 1
  fi
  chmod 644 "$completion"
done
manpage="$stage_path/mcp-repl.1"
if ! "$binary" --man > "$manpage"; then
  echo "release binary could not generate the manual page" >&2
  exit 1
fi
if [[ ! -f "$manpage" || -L "$manpage" || ! -s "$manpage" ]]; then
  echo "release binary generated an unsafe or empty manual page" >&2
  exit 1
fi
chmod 644 "$manpage"

# Python's standard libraries are present on every supported hosted runner and
# give GNU/Linux, macOS, and Windows one archive implementation. The writer
# below never walks the staging directory when choosing members: it validates
# one exact payload, sorts it by archive path, and assigns every metadata field.
# ZIP uses stored members and gzip uses raw stored DEFLATE blocks, so archive
# bytes do not depend on the host's zlib version or compression heuristics.
"$archive_python" - \
  "$extension" \
  "$archive_path" \
  "$stage_path" \
  "$stage" \
  "$binary_name" \
  "$source_date_epoch" <<'PYTHON'
import datetime
import os
import pathlib
import stat
import struct
import sys
import tarfile
import zipfile


extension, archive_arg, stage_arg, stage_name, binary_name, epoch_arg = sys.argv[1:]
archive = pathlib.Path(archive_arg)
stage = pathlib.Path(stage_arg)
epoch = int(epoch_arg)

expected_directories = {
    pathlib.Path("."),
    pathlib.Path("completions"),
}
expected_files = {
    pathlib.Path("LICENSE-APACHE"): 0o644,
    pathlib.Path("LICENSE-MIT"): 0o644,
    pathlib.Path("README.md"): 0o644,
    pathlib.Path("completions/mcp-repl.bash"): 0o644,
    pathlib.Path("completions/mcp-repl.fish"): 0o644,
    pathlib.Path("completions/mcp-repl.zsh"): 0o644,
    pathlib.Path("mcp-repl.1"): 0o644,
    pathlib.Path(binary_name): 0o755,
}

actual_directories = {pathlib.Path(".")}
actual_files = set()
for root, directories, files in os.walk(stage, followlinks=False):
    root_path = pathlib.Path(root)
    relative_root = root_path.relative_to(stage)
    for name in directories:
        path = root_path / name
        metadata = path.lstat()
        if not stat.S_ISDIR(metadata.st_mode):
            raise SystemExit(f"release payload contains a linked or non-directory entry: {path}")
        actual_directories.add(relative_root / name)
    for name in files:
        path = root_path / name
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode):
            raise SystemExit(f"release payload contains a linked or non-file entry: {path}")
        if metadata.st_size == 0:
            raise SystemExit(f"release payload contains an empty file: {path}")
        actual_files.add(relative_root / name)

if actual_directories != expected_directories or actual_files != set(expected_files):
    raise SystemExit("release payload is incomplete or contains an unexpected entry")

entries = [(f"{stage_name}/", stage, True, 0o755)]
entries.append((f"{stage_name}/completions/", stage / "completions", True, 0o755))
for relative, mode in expected_files.items():
    entries.append((f"{stage_name}/{relative.as_posix()}", stage / relative, False, mode))
entries.sort(key=lambda entry: entry[0])


def add_tar_entry(tar, archive_name, path, is_directory, mode):
    info = tarfile.TarInfo(archive_name)
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mode = mode
    info.mtime = epoch
    if is_directory:
        info.type = tarfile.DIRTYPE
        info.size = 0
        tar.addfile(info)
    else:
        info.type = tarfile.REGTYPE
        info.size = path.stat().st_size
        with path.open("rb") as source:
            tar.addfile(info, source)


def crc32_table():
    table = []
    for value in range(256):
        remainder = value
        for _ in range(8):
            if remainder & 1:
                remainder = (remainder >> 1) ^ 0xEDB88320
            else:
                remainder >>= 1
        table.append(remainder)
    return table


def write_stored_gzip(source_path, destination_path, timestamp):
    """Write one fixed-header gzip stream containing only stored blocks."""
    table = crc32_table()
    crc = 0xFFFFFFFF
    source_size = source_path.stat().st_size
    remaining = source_size

    with source_path.open("rb") as source, destination_path.open("xb") as destination:
        # ID1, ID2, CM=DEFLATE, FLG=0, MTIME, XFL=0, OS=unknown.
        destination.write(struct.pack("<BBBBIBB", 0x1F, 0x8B, 8, 0, timestamp, 0, 255))
        if source_size == 0:
            destination.write(b"\x01\x00\x00\xff\xff")
        while remaining:
            block = source.read(min(65535, remaining))
            if not block:
                raise SystemExit("deterministic gzip source ended unexpectedly")
            remaining -= len(block)
            final = remaining == 0
            length = len(block)
            # Stored blocks start on a byte boundary. The first byte contains
            # BFINAL and BTYPE=00 plus five fixed zero padding bits.
            destination.write(b"\x01" if final else b"\x00")
            destination.write(struct.pack("<HH", length, length ^ 0xFFFF))
            destination.write(block)
            for byte in block:
                crc = table[(crc ^ byte) & 0xFF] ^ (crc >> 8)
        if source.read(1):
            raise SystemExit("deterministic gzip source grew while packaging")
        destination.write(struct.pack("<II", crc ^ 0xFFFFFFFF, source_size & 0xFFFFFFFF))


if extension == "tar.gz":
    tar_path = archive.with_name(f"{archive.name}.payload.tar")
    with tarfile.open(tar_path, mode="x", format=tarfile.USTAR_FORMAT) as tar:
        for entry in entries:
            add_tar_entry(tar, *entry)
    write_stored_gzip(tar_path, archive, epoch)
    tar_path.unlink()
elif extension == "zip":
    timestamp = datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
    dos_timestamp = (
        timestamp.year,
        timestamp.month,
        timestamp.day,
        timestamp.hour,
        timestamp.minute,
        timestamp.second,
    )
    # The 0x5455 extended timestamp stores the exact (including odd-second)
    # epoch that the ZIP header's two-second DOS timestamp cannot represent.
    timestamp_extra = struct.pack("<HHBi", 0x5455, 5, 1, epoch)
    with zipfile.ZipFile(
        archive,
        mode="x",
        compression=zipfile.ZIP_STORED,
        allowZip64=False,
    ) as zipped:
        for archive_name, path, is_directory, mode in entries:
            info = zipfile.ZipInfo(archive_name, date_time=dos_timestamp)
            info.create_system = 3
            info.create_version = 20
            info.extract_version = 20
            info.flag_bits = 0x800
            info.extra = timestamp_extra
            info.external_attr = (stat.S_IFDIR if is_directory else stat.S_IFREG) | mode
            info.external_attr = (info.external_attr << 16) | (0x10 if is_directory else 0)
            payload = b"" if is_directory else path.read_bytes()
            zipped.writestr(
                info,
                payload,
                compress_type=zipfile.ZIP_STORED,
            )
else:
    raise SystemExit(f"unsupported archive extension: {extension}")
PYTHON

if [[ ! -f "$archive_path" || -L "$archive_path" || ! -s "$archive_path" ]]; then
  echo "deterministic archive writer did not produce a regular archive: $archive" >&2
  exit 1
fi
if command -v sha256sum > /dev/null; then
  digest=$(sha256sum "$archive_path" | awk '{print $1}')
else
  digest=$(shasum -a 256 "$archive_path" | awk '{print $1}')
fi
if [[ ! "$digest" =~ ^[0-9a-f]{64}$ ]]; then
  echo "could not compute canonical checksum for $archive" >&2
  exit 1
fi
printf '%s  %s\n' "$digest" "$archive" > "$checksum_path"

# Recheck immediately before installation so a stale or raced output is never
# overwritten by mv's default replacement behavior.
for output in "$stage" "$archive" "$archive.sha256"; do
  if [[ -e "$output" || -L "$output" ]]; then
    echo "release package output appeared while packaging: $output" >&2
    exit 1
  fi
done

# The temporary directory is deliberately on the output filesystem. A hard
# link therefore installs each complete file atomically and fails with EEXIST
# instead of exposing a partial file or relying on platform-specific mv -n.
install_exclusive() {
  "$archive_python" - "$1" "$2" <<'PYTHON'
import os
import sys


source, destination = sys.argv[1:]
installed = False
try:
    os.link(source, destination)
    installed = True
except FileExistsError:
    raise SystemExit(73)
try:
    os.unlink(source)
except BaseException:
    if installed:
        os.unlink(destination)
    raise
PYTHON
}

if ! install_exclusive "$archive_path" "$archive"; then
  echo "release archive destination was created concurrently: $archive" >&2
  exit 1
fi
archive_installed=true
if ! install_exclusive "$checksum_path" "$archive.sha256"; then
  echo "release checksum destination was created concurrently: $archive.sha256" >&2
  exit 1
fi
checksum_installed=true

printf '%s\n' "$archive"
