#!/usr/bin/env python3

import json
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
INSPECT = ROOT / "scripts/release/inspect-native-bundle-input.py"


class NativeBundleInputInspectionTests(unittest.TestCase):
    def inspect(self, bundle, target="x86_64-unknown-linux-gnu"):
        return subprocess.run([
            str(INSPECT),
            "--repository-root", str(ROOT),
            "--bundle", bundle,
            "--source-snapshot-hash", "a" * 64,
            "--target", target,
        ], text=True, capture_output=True, check=False)

    def test_source_only_bundle_requires_no_binary_build(self):
        result = self.inspect("central-auth", "portable")
        self.assertEqual(result.returncode, 0, result.stderr)
        plan = json.loads(result.stdout)
        self.assertFalse(plan["requires_binary_build"])
        self.assertEqual(plan["cargo_packages"], [])
        self.assertFalse(plan["ambient_target_reuse_allowed"])

    def test_source_only_bundle_rejects_architecture_specific_identity(self):
        result = self.inspect("central-auth")
        self.assertNotEqual(result.returncode, 0)

    def test_binary_bundle_uses_only_owned_packages(self):
        result = self.inspect("web")
        self.assertEqual(result.returncode, 0, result.stderr)
        plan = json.loads(result.stdout)
        self.assertEqual(plan["cargo_packages"], ["ryeos-web-tools"])
        self.assertEqual(plan["target"], {
            "kind": "triple", "triple": "x86_64-unknown-linux-gnu"
        })
        self.assertEqual(plan["project_path"], str(ROOT.resolve()))
        self.assertEqual([item["binary"] for item in plan["payloads"]], ["ryeos-web-tools"])

    def test_multi_payload_bundle_projects_all_packages_once(self):
        result = self.inspect("standard")
        self.assertEqual(result.returncode, 0, result.stderr)
        plan = json.loads(result.stdout)
        self.assertGreater(len(plan["payloads"]), 1)
        self.assertEqual(plan["cargo_packages"], sorted(set(
            payload["cargo_package"] for payload in plan["payloads"]
        )))
        self.assertIn("ryeos-handler-bins", plan["cargo_packages"])
        self.assertTrue(plan["requires_binary_build"])

    def test_distinct_bundles_are_independently_inspectable(self):
        web = json.loads(self.inspect("web").stdout)
        browser = json.loads(self.inspect("browser").stdout)
        self.assertEqual(web["bundle_name"], "web")
        self.assertEqual(browser["bundle_name"], "browser")
        self.assertNotEqual(web["payloads"], browser["payloads"])

    def test_core_is_reserved_for_substrate_release(self):
        result = self.inspect("core")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("substrate-owned", result.stderr)

    def test_rejects_floating_source_identity(self):
        result = subprocess.run([
            str(INSPECT),
            "--repository-root", str(ROOT),
            "--bundle", "web",
            "--source-snapshot-hash", "next",
            "--target", "x86_64-unknown-linux-gnu",
        ], text=True, capture_output=True, check=False)
        self.assertNotEqual(result.returncode, 0)

    def test_rejects_portable_binary_bundle(self):
        result = self.inspect("web", "portable")
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
