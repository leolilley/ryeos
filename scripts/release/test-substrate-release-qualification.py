#!/usr/bin/python3
"""Focused contract tests for the receipt-only substrate qualification assets."""
import io
import json
import os
from pathlib import Path
import runpy
import stat
import tempfile
import unittest
from unittest.mock import patch

import yaml

ROOT = Path(__file__).resolve().parents[2]
ASSET = ROOT / "bundles/bundle-release/.ai"
VERIFIER = ASSET / "tools/ryeos/bundle-release/substrate-qualify.py"


def receipt():
    return {
        "schema": "ryeos.substrate_build_receipt.v1",
        "kind": "substrate_build_receipt",
        "substrate_image_digest": "sha256:" + "a" * 64,
        "substrate_protocol": 1,
        "target": {"kind": "triple", "triple": "x86_64-unknown-linux-gnu"},
        "core_generation_hash": "b" * 64,
    }


class SubstrateQualificationTests(unittest.TestCase):
    def run_verifier(self, product, request=None):
        output = io.StringIO()
        real_path = Path

        def realized_path(*parts):
            if parts == ("/ryeos/realizations/substrate-release",):
                return product
            return real_path(*parts)

        environment = {
            "RYEOS_EXTERNAL_REALIZATIONS": json.dumps([
                {"id": "subject", "manifest_hash": "c" * 64}
            ]),
            "RYE_THREAD_ID": "qualification-thread",
        }
        with patch("pathlib.Path", side_effect=realized_path), \
             patch("sys.stdin", io.StringIO(json.dumps(request or {}))), \
             patch("sys.stdout", output), patch.dict(os.environ, environment, clear=False):
            runpy.run_path(str(VERIFIER), run_name="__main__")
        return json.loads(output.getvalue())

    def make_product(self, directory, value=None):
        product = Path(directory) / "substrate-release"
        (product / ".ai").mkdir(parents=True)
        encoded = json.dumps(value or receipt(), sort_keys=True, separators=(",", ":")).encode()
        (product / ".ai/substrate-release.json").write_bytes(encoded)
        return product

    def test_assets_close_the_fixed_coordinates(self):
        policy = yaml.safe_load((ASSET / "config/bundle-release/substrate-qualification.yaml").read_text())
        products = yaml.safe_load((ASSET / "config/bundle-release/substrate-build-products.yaml").read_text())
        tool = yaml.safe_load((ASSET / "tools/ryeos/bundle-release/substrate-qualify.yaml").read_text())
        graph = yaml.safe_load((ASSET / "graphs/ryeos/bundle-release/substrate-qualify.yaml").read_text())
        self.assertEqual(policy["product_qualification_policy"]["verifier_ref"], "tool:ryeos/bundle-release/substrate-qualify")
        relationship, = products["product_relationships"]["relationships"]
        self.assertEqual(relationship["name"], "substrate_release_to_qualification")
        self.assertEqual(relationship["producer"]["canonical_ref"], "graph:ryeos/bundle-release/substrate-build")
        self.assertEqual(relationship["qualification"]["policy_ref"], "config:bundle-release/substrate-qualification")
        for owner in (tool, graph):
            slot, = owner["external_product_slots"]
            self.assertEqual(slot["relationship_ref"], "config:bundle-release/substrate-build-products")
            self.assertEqual(slot["relationship"], "substrate_release_to_qualification")
            self.assertEqual(slot["mount"], "substrate-release")

    def test_canonical_receipt_only_tree_is_qualified(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_verifier(self.make_product(directory))
        self.assertEqual(result["claims"], ["substrate_release_checks_v1"])
        self.assertEqual(result["subject_manifest_hash"], "c" * 64)
        self.assertEqual(result["probe_evidence"]["receipt"], receipt())

    def test_noncanonical_receipt_bytes_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            product = self.make_product(directory)
            (product / ".ai/substrate-release.json").write_text(json.dumps(receipt(), indent=2))
            with self.assertRaisesRegex(SystemExit, "canonical JSON"):
                self.run_verifier(product)

    def test_open_receipt_and_extra_tree_entries_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            value = receipt()
            value["unreviewed"] = True
            product = self.make_product(directory, value)
            with self.assertRaisesRegex(SystemExit, "open or incomplete"):
                self.run_verifier(product)
        with tempfile.TemporaryDirectory() as directory:
            product = self.make_product(directory)
            (product / "payload").write_text("not receipt-only")
            with self.assertRaisesRegex(SystemExit, "receipt-only"):
                self.run_verifier(product)

    def test_executable_receipt_and_links_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            product = self.make_product(directory)
            receipt_path = product / ".ai/substrate-release.json"
            receipt_path.chmod(stat.S_IMODE(receipt_path.stat().st_mode) | 0o111)
            with self.assertRaisesRegex(SystemExit, "executable"):
                self.run_verifier(product)
        with tempfile.TemporaryDirectory() as directory:
            product = self.make_product(directory)
            (product / "alias").symlink_to(".ai/substrate-release.json")
            with self.assertRaisesRegex(SystemExit, "symbolic link"):
                self.run_verifier(product)


if __name__ == "__main__":
    unittest.main()
