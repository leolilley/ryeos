#!/usr/bin/env python3
"""Parity checks for the native-bundle-publication WP0 inventories."""

import json
from pathlib import Path
import subprocess
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests/fixtures/native-bundle-publication"
OWNERSHIP = ROOT / "scripts/release/bundle-payload-ownership.py"


def shell_lines(expression):
    result = subprocess.run(
        ["bash", "-c", f"source scripts/pkg/bundle-sets.sh; {expression}"],
        cwd=ROOT, text=True, capture_output=True, check=True,
    )
    return result.stdout.splitlines()


class NativeBundleWp0Tests(unittest.TestCase):
    def test_bundle_set_and_profile_fixture_matches_authority_source(self):
        fixture = json.loads((FIXTURES / "bundle-sets.json").read_text())
        self.assertEqual(set(shell_lines("ryeos_bundle_set_ids")), set(fixture["sets"]))
        for name, members in fixture["sets"].items():
            self.assertEqual(shell_lines(f"ryeos_bundle_set_names {name}"), members)
        self.assertEqual(set(shell_lines("ryeos_node_init_profile_names")), set(fixture["profiles"]))
        for profile, bundle_set in fixture["profiles"].items():
            self.assertEqual(shell_lines(f"ryeos_node_init_profile_bundle_set {profile}"), [bundle_set])
            document = yaml.safe_load(
                (ROOT / f"bundles/.ai/node/init/profiles/{profile}.yaml").read_text()
            )
            self.assertEqual(sorted(document["exact_bundles"]), sorted(fixture["sets"][bundle_set]))

    def test_payload_ownership_is_unique_and_references_known_bundles(self):
        fixture = json.loads((FIXTURES / "bundle-sets.json").read_text())
        known = set().union(*map(set, fixture["sets"].values()))
        seen = set()
        result = subprocess.run([str(OWNERSHIP), "--repository-root", str(ROOT)],
                                text=True, capture_output=True, check=True)
        for record in json.loads(result.stdout)["payloads"]:
            bundle, binary = record["bundle"], record["binary"]
            self.assertIn(bundle, known)
            self.assertTrue(record["cargo_package"])
            self.assertIn(record["build_class"], {"release", "static"})
            self.assertNotIn((bundle, binary), seen)
            seen.add((bundle, binary))
            for bundle_set in record["bundle_sets"]:
                self.assertTrue(bundle_set == "release-artifacts" or bundle_set in fixture["sets"])
                if bundle_set != "release-artifacts":
                    self.assertIn(bundle, fixture["sets"][bundle_set])

    def test_runtime_image_fixture_records_current_material_differences(self):
        fixture = json.loads((FIXTURES / "runtime-images.json").read_text())["targets"]
        self.assertTrue(fixture["ryeos-contained-workflow"]["security_boundary"])
        for target, record in fixture.items():
            dockerfile = (ROOT / record["dockerfile"]).read_text()
            self.assertIn(f"AS {target}", dockerfile) if record["dockerfile"] == "Dockerfile.release" else None
            if record["class"] == "workflow":
                self.assertTrue(record["node"], target)
                self.assertTrue(record["pip"], target)
                self.assertTrue(record["python_venv"], target)
                self.assertIn("python3-venv python3-pip", dockerfile)
                self.assertIn("FROM node:22-", dockerfile)
            if record["class"] in {"hosted", "contained"}:
                self.assertFalse(record["node"], target)
                self.assertFalse(record["pip"], target)
            if record["security_boundary"]:
                self.assertIn('io.ryeos.required-node-profile="contained-workflow"', dockerfile)
                self.assertIn("/usr/local/bin/lillux", dockerfile)


if __name__ == "__main__":
    unittest.main()
