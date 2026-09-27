#!/usr/bin/env python3
"""Focused tests for the receipt-only substrate release producer."""
import json
import pathlib
import subprocess
import tempfile
import unittest
import yaml

ROOT = pathlib.Path(__file__).resolve().parents[2]
TOOL = ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib/substrate-build.py"


def receipt():
    return {
        "schema": "ryeos.substrate_build_receipt.v1",
        "kind": "substrate_build_receipt",
        "substrate_image_digest": "sha256:" + "a" * 64,
        "substrate_protocol": 1,
        "target": {"kind": "triple", "triple": "x86_64-unknown-linux-gnu"},
        "core_generation_hash": "b" * 64,
    }


class SubstrateBuildTests(unittest.TestCase):
    def test_public_service_derives_receipt_from_verified_node_and_core(self):
        service = yaml.safe_load((ROOT / "bundles/bundle-release/.ai/services/bundle-release/substrate-build.yaml").read_text())
        self.assertNotIn("receipt", service["schema"])
        self.assertEqual(
            set(service["schema"]),
            {"project_path", "source_snapshot_hash", "catalog_namespace",
             "bundle_publication_policy_section_digest", "trust_epoch",
             "core_generation_hash", "core_generation_attestation_hash"},
        )
        handler = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs").read_text()
        self.assertIn("load_verified_substrate_identity", handler)
        self.assertIn("inspect_bundle_generation", handler)

    def run_tool(self, value):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(
                ["python3", str(TOOL)], cwd=directory,
                input=json.dumps(value), text=True, capture_output=True,
            )
            product = pathlib.Path(directory) / "products/substrate-release/.ai/substrate-release.json"
            contents = product.read_bytes() if product.exists() else None
            return result, contents

    def test_emits_exact_canonical_receipt(self):
        value = receipt()
        result, contents = self.run_tool({"receipt": value})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(contents, json.dumps(value, sort_keys=True, separators=(",", ":")).encode())

    def test_rejects_open_request(self):
        result, _ = self.run_tool({"receipt": receipt(), "extra": True})
        self.assertNotEqual(result.returncode, 0)

    def test_rejects_open_receipt(self):
        value = receipt()
        value["extra"] = True
        result, _ = self.run_tool({"receipt": value})
        self.assertNotEqual(result.returncode, 0)

    def test_rejects_zero_protocol(self):
        value = receipt()
        value["substrate_protocol"] = 0
        result, _ = self.run_tool({"receipt": value})
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
