#!/usr/bin/env python3

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
AUDIT = ROOT / "scripts/release/audit-bundle-generation.py"


class BundleGenerationAuditTests(unittest.TestCase):
    def run_audit(self, tree, *extra):
        result = subprocess.run(
            [str(AUDIT), "--root", str(tree), "--triple", "test-triple", *extra, "core"],
            text=True, capture_output=True, check=False,
        )
        return result, json.loads(result.stdout)

    def test_output_is_deterministic_and_accepts_closed_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = Path(directory)
            (tree / "core/.ai").mkdir(parents=True)
            (tree / "core/.ai/manifest.yaml").write_text("name: core\n")
            first, report = self.run_audit(tree)
            second, _ = self.run_audit(tree)
            self.assertEqual(first.returncode, 0)
            self.assertEqual(first.stdout, second.stdout)
            self.assertEqual(report["bundles"]["core"]["file_modes"], ["0644"])

    def test_rejects_symlink_and_unsupported_mode(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = Path(directory)
            (tree / "core").mkdir()
            target = tree / "core/private"
            target.write_text("x")
            target.chmod(0o600)
            (tree / "core/link").symlink_to("private")
            result, report = self.run_audit(tree)
            self.assertEqual(result.returncode, 1)
            self.assertTrue(any("symbolic links" in item for item in report["violations"]))
            self.assertTrue(any("unsupported file mode" in item for item in report["violations"]))

    def test_populated_mode_requires_declared_payloads(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = Path(directory)
            (tree / "core/.ai").mkdir(parents=True)
            result, report = self.run_audit(tree, "--require-populated")
            self.assertEqual(result.returncode, 1)
            self.assertTrue(any("missing populated executable" in item for item in report["violations"]))


if __name__ == "__main__":
    unittest.main()
