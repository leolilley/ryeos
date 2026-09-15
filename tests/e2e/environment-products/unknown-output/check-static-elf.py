#!/usr/bin/env python3
"""Refuse anything except a static Linux x86-64 ELF with no needed DSOs."""
import struct
import sys
from pathlib import Path


def check(path):
    binary = Path(path).read_bytes()
    if binary[:6] != b"\x7fELF\x02\x01" or len(binary) < 64:
        raise SystemExit("fixture verifier is not a little-endian ELF64 executable")
    if struct.unpack_from("<H", binary, 18)[0] != 62:
        raise SystemExit("fixture verifier is not Linux x86-64 machine code")
    offset = struct.unpack_from("<Q", binary, 32)[0]
    entry_size = struct.unpack_from("<H", binary, 54)[0]
    count = struct.unpack_from("<H", binary, 56)[0]
    if entry_size < 56 or offset + entry_size * count > len(binary):
        raise SystemExit("fixture verifier has an invalid program-header table")
    dynamic = []
    for index in range(count):
        header = offset + index * entry_size
        kind = struct.unpack_from("<I", binary, header)[0]
        if kind == 3:
            raise SystemExit("fixture verifier unexpectedly declares PT_INTERP")
        if kind == 2:
            section_offset = struct.unpack_from("<Q", binary, header + 8)[0]
            section_size = struct.unpack_from("<Q", binary, header + 32)[0]
            if section_offset + section_size > len(binary) or section_size % 16:
                raise SystemExit("fixture verifier has an invalid PT_DYNAMIC section")
            dynamic.append(binary[section_offset:section_offset + section_size])
    for section in dynamic:
        for cursor in range(0, len(section), 16):
            tag = struct.unpack_from("<q", section, cursor)[0]
            if tag == 0:
                break
            if tag == 1:
                raise SystemExit("fixture verifier unexpectedly declares DT_NEEDED")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} <verifier-binary>")
    check(sys.argv[1])
