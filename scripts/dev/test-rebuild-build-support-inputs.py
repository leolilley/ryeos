#!/usr/bin/env python3
"""Offline tests for rebuild-build-support-inputs.py: plan, verification, extraction."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
SCRIPT = Path(__file__).resolve().with_name("rebuild-build-support-inputs.py")
spec = importlib.util.spec_from_file_location("rebuild_build_support_inputs", SCRIPT)
rebuild = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rebuild)

CONTENT = {
    "bin/tee": (b"tee-bytes", 0o755),
    "elf/lib/libc.so.6": (b"libc-bytes", 0o644),
    "image/bin/bash": (b"bash-bytes", 0o755),
    "image/licenses/x-COPYRIGHT": (b"license", 0o644),
    "mystery/file": (b"who knows", 0o644),
}


def identity(data: bytes, mode: int) -> dict:
    return {"bytes": len(data), "mode": mode, "sha256": hashlib.sha256(data).hexdigest()}


def fake_pins() -> dict:
    inputs = {name: identity(*value) for name, value in CONTENT.items()}
    return {
        "inputs": inputs,
        "image": "docker.io/library/rust@sha256:" + "a" * 64,
        "image_paths": {"image/bin/bash": "/usr/bin/bash", "image/licenses/x-COPYRIGHT": "/usr/share/x"},
        "helper_member": "bin/tee",
        "tools_archive": {"name": "tools.tar.gz", "url": "https://example.invalid/tools.tar.gz",
                          "sha256": "b" * 64, "acquisition_sha256": "b" * 64, "max_bytes": 10},
        "source_archive": {"name": "src.tar.gz", "url": "https://example.invalid/src.tar.gz",
                           "sha256": "c" * 64, "prefix": "src-v1", "max_bytes": 10},
        "expected_manifest_hash": "d" * 64,
    }


def write_tree(root: Path, content: dict) -> None:
    for name, (data, mode) in content.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(mode)


def make_archive(path: Path, members: list) -> None:
    with tarfile.open(path, "w:gz") as archive:
        for name, kind, data in members:
            info = tarfile.TarInfo(name)
            if kind == "file":
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
            else:
                info.type = tarfile.SYMTYPE
                info.linkname = data.decode()
                archive.addfile(info)


class PlanTests(unittest.TestCase):
    def test_sources_and_unknown_members(self):
        plan = rebuild.build_plan(fake_pins())
        sources = {n: m["source"] for n, m in plan["members"].items()}
        self.assertEqual(sources, {
            "bin/tee": "tools-archive", "elf/lib/libc.so.6": "source-archive",
            "image/bin/bash": "image", "image/licenses/x-COPYRIGHT": "image",
            "mystery/file": "unknown"})
        self.assertEqual(plan["unknown_members"], ["mystery/file"])
        self.assertEqual(plan["members"]["elf/lib/libc.so.6"]["archive_member"], "src-v1/elf/lib/libc.so.6")
        self.assertEqual(plan["members"]["image/bin/bash"]["container_path"], "/usr/bin/bash")

    def test_image_member_without_recorded_path_is_unknown(self):
        pins = fake_pins()
        del pins["image_paths"]["image/bin/bash"]
        self.assertIn("image/bin/bash", rebuild.build_plan(pins)["unknown_members"])

    def test_execute_refuses_unknown_members_before_any_effect(self):
        pins = fake_pins()
        plan = rebuild.build_plan(pins)
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "out"
            with self.assertRaisesRegex(rebuild.Refusal, "unknown origin"):
                rebuild.execute(pins, plan, output, Path(tmp) / "scratch", False)
            self.assertFalse(output.exists())

    def test_conflicting_archive_pin_is_reported_and_refused(self):
        pins = fake_pins()
        pins["tools_archive"]["acquisition_sha256"] = "e" * 64
        pins["inputs"].pop("mystery/file")
        plan = rebuild.build_plan(pins)
        self.assertTrue(plan["sources"]["tools-archive"]["pin_conflict"])
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(rebuild.Refusal, "differs"):
                rebuild.execute(pins, plan, Path(tmp) / "out", Path(tmp) / "s", False)


class VerifyTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "tree"
        self.root.mkdir()
        write_tree(self.root, CONTENT)
        self.inputs = fake_pins()["inputs"]

    def tearDown(self):
        self.tmp.cleanup()

    def test_good_tree_and_manifest_agree_with_inventory(self):
        result = rebuild.verify_tree(self.root, self.inputs)
        self.assertEqual(result["members_verified"], len(CONTENT))
        self.assertEqual(result["manifest"], rebuild.inventory_manifest(self.inputs))
        # four directories (bin, elf, elf/lib, image, image/bin, image/licenses, mystery) + files
        self.assertEqual(result["manifest"]["entry_count"], len(CONTENT) + 7)

    def test_content_mismatch_refused(self):
        (self.root / "bin/tee").write_bytes(b"tee-bytez")
        (self.root / "bin/tee").chmod(0o755)
        with self.assertRaisesRegex(rebuild.Refusal, "sha256 mismatch"):
            rebuild.verify_tree(self.root, self.inputs)

    def test_size_mismatch_refused(self):
        (self.root / "bin/tee").write_bytes(b"short")
        with self.assertRaisesRegex(rebuild.Refusal, "size mismatch"):
            rebuild.verify_tree(self.root, self.inputs)

    def test_mode_mismatch_refused(self):
        (self.root / "bin/tee").chmod(0o644)
        with self.assertRaisesRegex(rebuild.Refusal, "mode mismatch"):
            rebuild.verify_tree(self.root, self.inputs)

    def test_missing_and_extra_members_refused(self):
        (self.root / "extra").write_bytes(b"x")
        with self.assertRaisesRegex(rebuild.Refusal, "membership"):
            rebuild.verify_tree(self.root, self.inputs)
        (self.root / "extra").unlink()
        (self.root / "bin/tee").unlink()
        with self.assertRaisesRegex(rebuild.Refusal, "membership"):
            rebuild.verify_tree(self.root, self.inputs)

    def test_symlink_refused(self):
        (self.root / "bin/tee").unlink()
        (self.root / "bin/tee").symlink_to("../image/bin/bash")
        with self.assertRaisesRegex(rebuild.Refusal, "link or special"):
            rebuild.verify_tree(self.root, self.inputs)

    def test_apply_modes_sets_inventory_modes(self):
        (self.root / "bin/tee").chmod(0o600)
        rebuild.apply_modes(self.root, self.inputs)
        self.assertEqual((self.root / "bin/tee").stat().st_mode & 0o777, 0o755)


class ExtractTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.inputs = fake_pins()["inputs"]

    def tearDown(self):
        self.tmp.cleanup()

    def test_extracts_only_listed_members(self):
        archive = self.dir / "a.tar.gz"
        make_archive(archive, [("src-v1/elf/lib/libc.so.6", "file", b"libc-bytes"),
                               ("src-v1/other", "file", b"unlisted")])
        out = self.dir / "out"
        out.mkdir()
        wanted = {"src-v1/elf/lib/libc.so.6": ("elf/lib/libc.so.6", self.inputs["elf/lib/libc.so.6"])}
        rebuild.extract_listed(archive, wanted, out)
        self.assertEqual([p.relative_to(out).as_posix() for p in out.rglob("*") if p.is_file()],
                         ["elf/lib/libc.so.6"])

    def test_wrong_bytes_symlink_traversal_and_absent_refused(self):
        out = self.dir / "out"
        out.mkdir()
        wanted = {"src-v1/elf/lib/libc.so.6": ("elf/lib/libc.so.6", self.inputs["elf/lib/libc.so.6"])}
        cases = {
            "differs": [("src-v1/elf/lib/libc.so.6", "file", b"libc-bytez")],
            "not a regular": [("src-v1/elf/lib/libc.so.6", "link", b"/etc/passwd")],
            "unsafe member": [("../escape", "file", b"x"), ("src-v1/elf/lib/libc.so.6", "file", b"libc-bytes")],
            "lacks listed": [("src-v1/unrelated", "file", b"x")],
        }
        for pattern, members in cases.items():
            archive = self.dir / f"{abs(hash(pattern))}.tar.gz"
            make_archive(archive, members)
            with self.assertRaisesRegex(rebuild.Refusal, pattern):
                rebuild.extract_listed(archive, wanted, out)
        self.assertFalse((self.dir / "escape").exists())

    def test_helper_located_by_hash_among_same_named_members(self):
        archive = self.dir / "t.tar.gz"
        make_archive(archive, [("pkg/old/tee", "file", b"different"),
                               ("pkg/bin/tee", "file", b"tee-bytes")])
        target = self.dir / "out/bin/tee"
        found = rebuild.extract_by_hash(archive, "tee", target, self.inputs["bin/tee"])
        self.assertEqual(found, "pkg/bin/tee")
        self.assertEqual(target.read_bytes(), b"tee-bytes")
        make_archive(archive, [("pkg/bin/tee", "file", b"tee-bytez")])
        with self.assertRaisesRegex(rebuild.Refusal, "no archive member"):
            rebuild.extract_by_hash(archive, "tee", self.dir / "o2", self.inputs["bin/tee"])

    def test_safe_relative_rejects_traversal(self):
        for bad in ("/abs", "a/../b", "a//b", "..", ""):
            with self.assertRaises(rebuild.Refusal):
                rebuild.safe_relative(bad)


class RepositoryTests(unittest.TestCase):
    def test_real_inventory_loads_and_plans_every_member(self):
        pins = rebuild.load_pins()
        plan = rebuild.build_plan(pins)
        self.assertEqual(plan["unknown_members"], [])
        self.assertRegex(plan["expected_manifest_hash"], r"^[0-9a-f]{64}$")
        self.assertEqual(plan["member_count"], len(pins["inputs"]))


if __name__ == "__main__":
    unittest.main()
