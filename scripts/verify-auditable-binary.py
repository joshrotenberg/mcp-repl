#!/usr/bin/env python3
"""Verify cargo-auditable inventory in a native release executable."""

from __future__ import annotations

import argparse
import json
import struct
import sys
import zlib
from pathlib import Path


AUDIT_SECTION = b".dep-v0"
MAX_INVENTORY_SIZE = 8 * 1024 * 1024


class VerificationError(ValueError):
    """The executable does not contain a valid cargo-auditable inventory."""


def require_slice(data: bytes, offset: int, size: int, description: str) -> bytes:
    if offset < 0 or size < 0 or offset > len(data) or size > len(data) - offset:
        raise VerificationError(f"truncated {description}")
    return data[offset : offset + size]


def unpack_from(fmt: str, data: bytes, offset: int, description: str) -> tuple[int, ...]:
    size = struct.calcsize(fmt)
    try:
        return struct.unpack(fmt, require_slice(data, offset, size, description))
    except struct.error as error:
        raise VerificationError(f"invalid {description}") from error


def terminated_name(raw: bytes, description: str) -> bytes:
    name = raw.split(b"\0", 1)[0]
    if not name:
        raise VerificationError(f"empty {description}")
    return name


def elf_audit_sections(data: bytes) -> list[bytes]:
    if require_slice(data, 0, 16, "ELF identification")[:4] != b"\x7fELF":
        raise VerificationError("invalid ELF magic")
    identification = data[:16]
    if identification[4] != 2 or identification[5] != 1:
        raise VerificationError("release verifier supports only little-endian ELF64")

    section_offset = unpack_from("<Q", data, 40, "ELF section offset")[0]
    section_size = unpack_from("<H", data, 58, "ELF section size")[0]
    section_count = unpack_from("<H", data, 60, "ELF section count")[0]
    names_index = unpack_from("<H", data, 62, "ELF name-table index")[0]
    if section_size < 64 or section_count < 2 or names_index >= section_count:
        raise VerificationError("invalid or extended ELF section table")
    require_slice(
        data,
        section_offset,
        section_size * section_count,
        "ELF section table",
    )

    def section(index: int) -> tuple[int, int, int]:
        header = section_offset + index * section_size
        name_offset = unpack_from("<I", data, header, "ELF section name")[0]
        file_offset = unpack_from("<Q", data, header + 24, "ELF section file offset")[0]
        file_size = unpack_from("<Q", data, header + 32, "ELF section file size")[0]
        return name_offset, file_offset, file_size

    _, names_offset, names_size = section(names_index)
    names = require_slice(data, names_offset, names_size, "ELF section-name table")

    result: list[bytes] = []
    for index in range(1, section_count):
        name_offset, file_offset, file_size = section(index)
        if name_offset >= len(names):
            raise VerificationError("ELF section name is outside its string table")
        name_end = names.find(b"\0", name_offset)
        if name_end < 0:
            raise VerificationError("unterminated ELF section name")
        if names[name_offset:name_end] == AUDIT_SECTION:
            result.append(require_slice(data, file_offset, file_size, "ELF audit section"))
    return result


def macho_audit_sections(data: bytes) -> list[bytes]:
    if require_slice(data, 0, 4, "Mach-O magic") != b"\xcf\xfa\xed\xfe":
        raise VerificationError("release verifier supports only little-endian Mach-O 64")
    command_count, command_bytes = unpack_from(
        "<II", data, 16, "Mach-O load-command header"
    )
    command_offset = 32
    commands_end = command_offset + command_bytes
    require_slice(data, command_offset, command_bytes, "Mach-O load commands")

    result: list[bytes] = []
    for _ in range(command_count):
        command, command_size = unpack_from(
            "<II", data, command_offset, "Mach-O load command"
        )
        if command_size < 8 or command_offset + command_size > commands_end:
            raise VerificationError("invalid Mach-O load-command size")
        if command == 0x19:  # LC_SEGMENT_64
            if command_size < 72:
                raise VerificationError("truncated Mach-O segment command")
            segment_name = terminated_name(
                require_slice(data, command_offset + 8, 16, "Mach-O segment name"),
                "Mach-O segment name",
            )
            section_count = unpack_from(
                "<I", data, command_offset + 64, "Mach-O section count"
            )[0]
            if section_count > (command_size - 72) // 80:
                raise VerificationError("Mach-O sections exceed their segment command")
            for index in range(section_count):
                section_offset = command_offset + 72 + index * 80
                section_name = terminated_name(
                    require_slice(data, section_offset, 16, "Mach-O section name"),
                    "Mach-O section name",
                )
                section_segment = terminated_name(
                    require_slice(data, section_offset + 16, 16, "Mach-O section segment"),
                    "Mach-O section segment",
                )
                if section_name == AUDIT_SECTION:
                    if segment_name != b"__DATA" or section_segment != b"__DATA":
                        raise VerificationError("Mach-O audit section is outside __DATA")
                    file_size = unpack_from(
                        "<Q", data, section_offset + 40, "Mach-O section size"
                    )[0]
                    file_offset = unpack_from(
                        "<I", data, section_offset + 48, "Mach-O section offset"
                    )[0]
                    result.append(
                        require_slice(data, file_offset, file_size, "Mach-O audit section")
                    )
        command_offset += command_size
    if command_offset != commands_end:
        raise VerificationError("Mach-O load-command sizes do not match the header")
    return result


def pe_audit_sections(data: bytes) -> list[bytes]:
    if require_slice(data, 0, 2, "DOS header") != b"MZ":
        raise VerificationError("invalid PE DOS magic")
    pe_offset = unpack_from("<I", data, 60, "PE header offset")[0]
    if require_slice(data, pe_offset, 4, "PE signature") != b"PE\0\0":
        raise VerificationError("invalid PE signature")
    section_count = unpack_from("<H", data, pe_offset + 6, "PE section count")[0]
    optional_size = unpack_from("<H", data, pe_offset + 20, "PE optional-header size")[0]
    if section_count == 0:
        raise VerificationError("PE executable has no sections")
    section_table = pe_offset + 24 + optional_size
    require_slice(data, section_table, section_count * 40, "PE section table")

    result: list[bytes] = []
    for index in range(section_count):
        section_offset = section_table + index * 40
        name = terminated_name(
            require_slice(data, section_offset, 8, "PE section name"),
            "PE section name",
        )
        if name == AUDIT_SECTION:
            file_size, file_offset = unpack_from(
                "<II", data, section_offset + 16, "PE section location"
            )
            result.append(require_slice(data, file_offset, file_size, "PE audit section"))
    return result


def audit_section(data: bytes) -> bytes:
    if data.startswith(b"\x7fELF"):
        sections = elf_audit_sections(data)
    elif data.startswith(b"\xcf\xfa\xed\xfe"):
        sections = macho_audit_sections(data)
    elif data.startswith(b"MZ"):
        sections = pe_audit_sections(data)
    else:
        raise VerificationError("unsupported native executable format")
    if len(sections) != 1 or not sections[0]:
        raise VerificationError("executable must contain exactly one non-empty .dep-v0 section")
    return sections[0]


def decode_inventory(compressed: bytes) -> dict[str, object]:
    try:
        decompressor = zlib.decompressobj()
        decoded = decompressor.decompress(compressed, MAX_INVENTORY_SIZE + 1)
        if len(decoded) > MAX_INVENTORY_SIZE or decompressor.unconsumed_tail:
            raise VerificationError("cargo-auditable inventory exceeds 8 MiB")
        decoded += decompressor.flush(MAX_INVENTORY_SIZE + 1 - len(decoded))
    except zlib.error as error:
        raise VerificationError("cargo-auditable inventory is not valid zlib data") from error
    if len(decoded) > MAX_INVENTORY_SIZE:
        raise VerificationError("cargo-auditable inventory exceeds 8 MiB")
    if not decompressor.eof:
        raise VerificationError("cargo-auditable inventory is truncated")
    if decompressor.unused_data.strip(b"\0"):
        raise VerificationError("unexpected data follows cargo-auditable inventory")
    try:
        inventory = json.loads(decoded.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise VerificationError("cargo-auditable inventory is not valid UTF-8 JSON") from error
    if not isinstance(inventory, dict):
        raise VerificationError("cargo-auditable inventory root must be an object")
    return inventory


def verify_inventory(inventory: dict[str, object], package: str, version: str) -> None:
    inventory_format = inventory.get("format")
    if type(inventory_format) is not int or inventory_format not in (1, 8):
        raise VerificationError("unsupported cargo-auditable inventory format")
    packages = inventory.get("packages")
    if not isinstance(packages, list) or len(packages) < 2:
        raise VerificationError("cargo-auditable inventory has no dependency packages")

    roots: list[tuple[int, dict[str, object]]] = []
    for index, entry in enumerate(packages):
        if not isinstance(entry, dict):
            raise VerificationError("cargo-auditable package entry must be an object")
        for field in ("name", "version", "source"):
            value = entry.get(field)
            if not isinstance(value, str) or not value:
                raise VerificationError(f"cargo-auditable package has invalid {field}")
        kind = entry.get("kind", "runtime")
        if kind not in ("runtime", "build"):
            raise VerificationError("cargo-auditable package has invalid dependency kind")
        root_value = entry.get("root", False)
        if type(root_value) is not bool:
            raise VerificationError("cargo-auditable package has invalid root marker")
        if root_value:
            roots.append((index, entry))
        dependencies = entry.get("dependencies", [])
        if not isinstance(dependencies, list):
            raise VerificationError("cargo-auditable package dependencies must be an array")
        seen_dependencies: set[int] = set()
        for dependency in dependencies:
            if (
                type(dependency) is not int
                or dependency < 0
                or dependency >= len(packages)
                or dependency == index
            ):
                raise VerificationError("cargo-auditable dependency index is invalid")
            if dependency in seen_dependencies:
                raise VerificationError("cargo-auditable package has duplicate dependencies")
            seen_dependencies.add(dependency)

    if len(roots) != 1:
        raise VerificationError("cargo-auditable inventory must have exactly one root package")
    root_index, root = roots[0]
    if root.get("name") != package or root.get("version") != version:
        raise VerificationError(
            f"cargo-auditable root does not match {package} {version}"
        )
    reachable: set[int] = set()
    pending = [root_index]
    while pending:
        index = pending.pop()
        if index in reachable:
            continue
        reachable.add(index)
        entry = packages[index]
        pending.extend(entry.get("dependencies", []))
    if len(reachable) != len(packages):
        raise VerificationError(
            "cargo-auditable inventory contains packages unreachable from its root"
        )


def verify(path: Path, package: str, version: str) -> None:
    if not path.is_file() or path.is_symlink():
        raise VerificationError(f"binary is missing or linked: {path}")
    data = path.read_bytes()
    verify_inventory(decode_inventory(audit_section(data)), package, version)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("package")
    parser.add_argument("version")
    args = parser.parse_args()
    try:
        verify(args.binary, args.package, args.version)
    except (OSError, VerificationError) as error:
        print(f"auditable binary verification failed: {error}", file=sys.stderr)
        return 1
    print(f"verified cargo-auditable inventory: {args.binary}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
