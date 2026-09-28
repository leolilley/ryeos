#!/usr/bin/env python3
"""Installer contract tests; no Docker daemon or administrator writes."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "installer", Path(__file__).with_name("install-contained-docker-runtime.py")
)
INSTALLER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INSTALLER)


class InstallerTests(unittest.TestCase):
    def test_preserves_default_and_unrelated_settings_without_mutating_input(self):
        original = {"default-runtime": "runc", "data-root": "/data/docker", "runtimes": {"other": {"path": "/usr/bin/other"}}}
        snapshot = json.dumps(original, sort_keys=True)
        result = INSTALLER.merge_config(original, "/usr/lib/ryeos/hook", "/usr/bin/runc")
        self.assertEqual(result["default-runtime"], "runc")
        self.assertEqual(result["data-root"], "/data/docker")
        self.assertEqual(result["runtimes"]["other"], original["runtimes"]["other"])
        self.assertEqual(json.dumps(original, sort_keys=True), snapshot)
        self.assertEqual(INSTALLER.merge_config(result, "/usr/lib/ryeos/hook", "/usr/bin/runc"), result)

    def test_different_registration_requires_explicit_migration(self):
        first = INSTALLER.merge_config({}, "/usr/lib/ryeos/old", "/usr/bin/runc")
        with self.assertRaisesRegex(ValueError, "migration"):
            INSTALLER.merge_config(first, "/usr/lib/ryeos/new", "/usr/bin/runc")

    def test_ambiguous_or_malformed_configuration_refuses(self):
        with self.assertRaisesRegex(ValueError, "duplicate"):
            json.loads('{"runtimes":{},"runtimes":{}}', object_pairs_hook=INSTALLER.unique_object)
        for original in [[], {"runtimes": None}, {"runtimes": []}]:
            with self.assertRaises(ValueError):
                INSTALLER.merge_config(original, "/hook", "/runc")

    def test_file_reader_rejects_symlink_hardlink_and_oversize(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "binary"
            path.write_bytes(b"pinned")
            self.assertEqual(INSTALLER.read_regular(path), b"pinned")
            with self.assertRaises(ValueError):
                INSTALLER.read_regular(path, maximum=3)
            link = Path(root) / "link"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                INSTALLER.read_regular(link)
            hard = Path(root) / "hard"
            hard.hardlink_to(path)
            with self.assertRaises(ValueError):
                INSTALLER.read_regular(path)

    def test_cargo_hardlinked_input_is_allowed_only_as_unprotected_snapshot(self):
        with tempfile.TemporaryDirectory() as root:
            artifact = Path(root) / "deps-artifact"
            artifact.write_bytes(b"compiled artifact")
            binary = Path(root) / "binary"
            binary.hardlink_to(artifact)
            snapshot = INSTALLER.read_regular(binary, allow_hardlinks=True)
            self.assertEqual(snapshot, b"compiled artifact")
            artifact.write_bytes(b"later modification")
            self.assertEqual(snapshot, b"compiled artifact")
            with self.assertRaisesRegex(ValueError, "exactly one hard link"):
                INSTALLER.read_regular(binary, protected=True, allow_hardlinks=True)


if __name__ == "__main__":
    unittest.main()
