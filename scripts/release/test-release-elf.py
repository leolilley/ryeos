#!/usr/bin/env python3
"""Focused final-payload ELF contract and byte-preservation regressions."""
import importlib.util
import os
import pathlib
import struct
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "release_elf", ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib/release-elf.py")
ELF = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ELF)


def fixture(interpreter=None, extra=(), needed=True):
    data = bytearray(1024)
    data[:16] = b"\x7fELF\x02\x01\x01" + bytes(9)
    headers = [(1, 5, 0, 0x400000, 0, len(data), len(data), 4096)]
    if interpreter is not None:
        encoded = interpreter.encode() + b"\0"
        data[256:256 + len(encoded)] = encoded
        headers.append((3, 4, 256, 0x400100, 0, len(encoded), len(encoded), 1))
    entries = [(5, 0x400300), (10, 32)]
    if needed:
        entries.append((1, 0))
    entries.extend(extra)
    entries.append((0, 0))
    for i, entry in enumerate(entries):
        struct.pack_into("<QQ", data, 512 + 16 * i, *entry)
    data[768:778] = b"libc.so.6\0"
    headers.append((2, 6, 512, 0x400200, 0, 16 * len(entries), 16 * len(entries), 8))
    struct.pack_into("<HHIQQQIHHHHHH", data, 16,
                     3, 62, 1, 0, 64, 0, 0, 64, 56, len(headers), 0, 0, 0)
    for i, header in enumerate(headers):
        struct.pack_into("<IIQQQQQQ", data, 64 + i * 56, *header)
    return data


class ReleaseElfTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = pathlib.Path(self.scratch.name)
        self.platform = self.root / "platform"
        (self.platform / "lib").mkdir(parents=True)
        (self.platform / "lib/libc.so.6").touch()
        self.expected = str(self.platform / "lib/ld-linux-x86-64.so.2")
        self.path = self.root / "payload"

    def reject(self, data, build_class="release"):
        self.path.write_bytes(data)
        with self.assertRaises((ValueError, UnicodeError)):
            ELF.normalize_output_elf(self.path, build_class, self.platform)
        self.assertEqual(self.path.read_bytes(), data)

    def test_normalization_preserves_every_other_byte_and_allocation(self):
        data = fixture(self.expected)
        self.path.write_bytes(data)
        self.path.chmod(0o755)
        identity = ELF.normalize_output_elf(self.path, "release", self.platform)
        output = self.path.read_bytes()
        size = len(self.expected.encode()) + 1
        self.assertEqual(output[:256], data[:256])
        self.assertEqual(output[256 + size:], data[256 + size:])
        self.assertEqual(output[256:256 + size],
                         (ELF.SUBSTRATE_INTERPRETER.encode() + b"\0").ljust(size, b"\0"))
        self.assertNotEqual(identity["before_sha256"], identity["after_sha256"])
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o755)
        ELF.verify_output_elf(self.path, "release", self.platform)

    def test_static_is_unchanged(self):
        data = fixture(needed=False)
        self.path.write_bytes(data)
        identity = ELF.normalize_output_elf(self.path, "static", self.platform)
        self.assertEqual(identity["before_sha256"], identity["after_sha256"])
        self.assertEqual(self.path.read_bytes(), data)

    def test_static_rejects_interpreter_or_dependency(self):
        self.reject(fixture(self.expected, needed=False), "static")
        self.reject(fixture(), "static")

    def test_rejects_search_paths_even_empty_and_nodeflib(self):
        for entry in ((15, 10), (29, 10), (0x6ffffffb, 0x800)):
            with self.subTest(entry=entry):
                self.reject(fixture(self.expected, (entry,)))

    def test_rejects_unexpected_original_interpreter(self):
        self.reject(fixture(ELF.SUBSTRATE_INTERPRETER))
        self.reject(fixture("/other/loader"))

    def test_rejects_missing_or_symlink_dependency(self):
        library = self.platform / "lib/libc.so.6"
        library.unlink()
        self.reject(fixture(self.expected))
        library.symlink_to(self.path)
        self.reject(fixture(self.expected))

    def test_rejects_malformed_headers_and_segment_bounds(self):
        cases = [bytearray(10), fixture(self.expected)]
        cases[1][4] = 1
        for offset, fmt, value in ((18, "H", 3), (52, "H", 0), (54, "H", 0),
                                   (56, "H", 65535), (32, "Q", 1000),
                                   (64 + 56 + 8, "Q", 1020),
                                   (64 + 56 + 32, "Q", 2048)):
            data = fixture(self.expected)
            struct.pack_into("<" + fmt, data, offset, value)
            cases.append(data)
        for data in cases:
            with self.subTest(data=data[:64]):
                self.reject(data)

    def test_duplicate_interpreter(self):
        data = fixture(self.expected)
        data[176:232] = data[120:176]
        self.reject(data)

    def test_rejects_invalid_section_table(self):
        data = fixture(self.expected)
        struct.pack_into("<Q", data, 40, 1000)
        struct.pack_into("<HH", data, 58, 64, 1)
        self.reject(data)

    def test_rejects_interpreter_overlapping_strings(self):
        data = fixture(self.expected)
        struct.pack_into("<Q", data, 520, 0x400100)
        self.reject(data)

    def test_rejects_nonprivate_paths_without_modifying_target(self):
        data = fixture(self.expected)
        target = self.root / "original"
        target.write_bytes(data)
        self.path.symlink_to(target)
        with self.assertRaisesRegex(ValueError, "private regular"):
            ELF.normalize_output_elf(self.path, "release", self.platform)
        self.assertEqual(target.read_bytes(), data)
        self.path.unlink()
        os.link(target, self.path)
        with self.assertRaisesRegex(ValueError, "private regular"):
            ELF.normalize_output_elf(self.path, "release", self.platform)
        self.assertEqual(target.read_bytes(), data)
        self.path.unlink()
        self.path.mkdir()
        with self.assertRaisesRegex(ValueError, "private regular"):
            ELF.normalize_output_elf(self.path, "release", self.platform)

    def test_rejects_short_interpreter_allocation(self):
        data = fixture("/lib/ld-linux-x86-64.so.2")
        self.path.write_bytes(data)
        with self.assertRaisesRegex(ValueError, "exceeds allocation"):
            ELF.normalize_output_elf(self.path, "release", "/")
        self.assertEqual(self.path.read_bytes(), data)

    def test_interpreter_requires_terminated_zero_filled_allocation(self):
        data = fixture(self.expected)
        data[256 + len(self.expected)] = 1
        self.reject(data)
        data = fixture(self.expected)
        data[260] = 0
        self.reject(data)

    def test_dynamic_string_bounds_and_termination(self):
        for entry in ((1, 1024), (5, 0x500000), (10, 2048)):
            self.reject(fixture(self.expected, (entry,)))
        data = fixture(self.expected)
        data[768:800] = b"x" * 32
        self.reject(data)

    def test_stripped_static_without_dynamic_segment(self):
        data = fixture(needed=False)
        struct.pack_into("<H", data, 56, 1)
        self.path.write_bytes(data)
        ELF.verify_output_elf(self.path, "static", self.platform)


if __name__ == "__main__":
    unittest.main()
