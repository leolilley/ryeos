#!/usr/bin/env python3
"""Focused contract tests for purpose-owned Core seed assets."""
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
QUALIFIER = ASSET / "tools/ryeos/bundle-release/core-seed-qualify.py"
CAPTURE = ASSET / "tools/ryeos/bundle-release/core-seed-capture.py"
BUILD = ASSET / "tools/ryeos/bundle-release/core-seed-build.py"


def release_input(name="core"):
    return {
        "schema": "ryeos.bundle_release_input_plan.v1",
        "project_path": "/source",
        "bundle_name": name,
        "authored_manifest": {"name": name, "version": "1.0.0",
                              "provides_kinds": [], "requires_kinds": []},
        "source_snapshot_hash": "a" * 64,
        "predecessor_generation_hash": None,
        "target": {"kind": "triple", "triple": "x86_64-unknown-linux-gnu"},
        "build_profile": "release",
        "payload_ownership_item_ref": "config:bundle-release/payload-ownership",
        "payload_ownership_content_hash": "b" * 64,
        "payloads": [], "cargo_packages": [], "build_classes": [],
        "requires_binary_build": True,
        "clean_output_required": True,
        "ambient_target_reuse_allowed": False,
    }


def signed_manifest(name="core"):
    body = json.dumps({"name": name, "provides_kinds": [],
                       "requires_kinds": [], "version": "1.0.0"},
                      sort_keys=True, separators=(",", ":"))
    return "# ryeos:signed:fixture\n" + body + "\n"


class CoreSeedAssetTests(unittest.TestCase):
    def test_bootstrap_graph_closes_genesis_without_an_image_rebuild(self):
        graph = yaml.safe_load(
            (ASSET / "graphs/ryeos/bundle-release/bootstrap.yaml").read_text()
        )
        nodes = graph["config"]["nodes"]
        ordered_services = [
            nodes[name]["action"]["item_id"]
            for name in nodes
            if "action" in nodes[name]
        ]
        self.assertEqual(ordered_services, [
            "service:bundle-release/core-seed-inspect",
            "service:bundle-release/core-seed-build",
            "service:bundle-release/request-tree-signing",
            "service:bundle-release/core-seed-capture",
            "service:bundle-release/core-seed-qualify",
            "service:bundle-release/generation-finalize",
            "service:bundle-release/request-authorization",
            "service:bundle-release/substrate-build",
            "service:bundle-release/substrate-qualify",
            "service:bundle-release/substrate-release-finalize",
            "service:bundle-release/substrate-release-authorization",
            "service:bundle-release/genesis-set-compose",
            "service:bundle-release/catalog-request-publication",
            "service:bundle-release/catalog-remote-publish",
        ])
        substrate = nodes["build_substrate_receipt"]["action"]["params"]
        self.assertNotIn("receipt", substrate)
        self.assertEqual(substrate["core_generation_hash"],
                         "${state.core_generation.generation_hash}")
        catalog = nodes["authorize_catalog_genesis"]["action"]["params"]
        self.assertIsNone(catalog["predecessor_publication_attestation_hash"])
        self.assertEqual(catalog["expected_sequence"], 0)
        remote = nodes["publish_catalog_genesis"]["action"]["params"]
        self.assertIsNone(remote["expected_catalog_head"])

    def test_assets_close_the_exact_rust_coordinates(self):
        build = yaml.safe_load((ASSET / "config/bundle-release/core-seed-build-products.yaml").read_text())
        capture = yaml.safe_load((ASSET / "config/bundle-release/core-seed-capture-products.yaml").read_text())
        policy = yaml.safe_load((ASSET / "config/bundle-release/core-seed-qualification.yaml").read_text())
        build_graph = yaml.safe_load((ASSET / "graphs/ryeos/bundle-release/core-seed-build.yaml").read_text())
        capture_graph = yaml.safe_load((ASSET / "graphs/ryeos/bundle-release/core-seed-capture.yaml").read_text())
        qualifier = yaml.safe_load((ASSET / "tools/ryeos/bundle-release/core-seed-qualify.yaml").read_text())
        build_relation, = build["product_relationships"]["relationships"]
        capture_relation, = capture["product_relationships"]["relationships"]
        self.assertEqual(build_relation["producer"]["canonical_ref"],
                         "graph:ryeos/bundle-release/core-seed-build")
        self.assertEqual(build_relation["consumer"], {
            "canonical_ref": "graph:ryeos/bundle-release/core-seed-capture",
            "declaration_id": "unsigned_core",
        })
        self.assertEqual(capture_relation["producer"]["canonical_ref"],
                         "graph:ryeos/bundle-release/core-seed-capture")
        self.assertEqual(capture_relation["consumer"], {
            "canonical_ref": "tool:ryeos/bundle-release/core-seed-qualify",
            "declaration_id": "subject",
        })
        self.assertEqual(capture_relation["qualification"], {
            "policy_ref": "config:bundle-release/core-seed-qualification",
            "required_claims": ["substrate_core_seed_checks_v1"],
        })
        self.assertEqual(policy["product_qualification_policy"]["allowed_claims"],
                         ["substrate_core_seed_checks_v1"])
        self.assertEqual(build_graph["product_recipe"],
                         "config:bundle-release/core-seed-build-products")
        self.assertEqual(capture_graph["product_recipe"],
                         "config:bundle-release/core-seed-capture-products")
        for owner, slot_id, relationship, mount in (
            (capture_graph, "unsigned_core", "core_seed_to_signed_capture", "unsigned-core-seed"),
            (qualifier, "subject", "signed_core_seed_to_qualification", "core-seed"),
        ):
            slot, = owner["external_product_slots"]
            self.assertEqual((slot["id"], slot["relationship"], slot["mount"]),
                             (slot_id, relationship, mount))

    def make_core(self, directory, name="core", executable=True):
        root = Path(directory) / "core-seed"
        (root / ".ai/bin/x86_64-unknown-linux-gnu").mkdir(parents=True)
        (root / ".ai/manifest.yaml").write_text(signed_manifest(name))
        binary = root / ".ai/bin/x86_64-unknown-linux-gnu/ryeos-core-tools"
        binary.write_bytes(b"core")
        binary.chmod(0o755 if executable else 0o644)
        return root

    def run_qualifier(self, product, request=None):
        output = io.StringIO()
        real_path = Path

        def realized_path(*parts):
            if parts == ("/ryeos/realizations/core-seed",):
                return product
            return real_path(*parts)

        environment = {
            "RYEOS_EXTERNAL_REALIZATIONS": json.dumps([
                {"id": "subject", "manifest_hash": "c" * 64}
            ]),
            "RYE_THREAD_ID": "core-qualification-thread",
        }
        with patch("pathlib.Path", side_effect=realized_path), \
             patch("sys.stdin", io.StringIO(json.dumps(request or {}))), \
             patch("sys.stdout", output), patch.dict(os.environ, environment, clear=False):
            runpy.run_path(str(QUALIFIER), run_name="__main__")
        return json.loads(output.getvalue())

    def test_exact_signed_core_seed_is_qualified(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_qualifier(self.make_core(directory))
        self.assertEqual(result["claims"], ["substrate_core_seed_checks_v1"])
        self.assertEqual(result["subject_manifest_hash"], "c" * 64)
        self.assertEqual(result["probe_evidence"]["bundle_name"], "core")
        self.assertEqual(result["probe_evidence"]["binary_count"], 1)

    def test_noncore_and_nonexecutable_seed_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(SystemExit, "identify Core"):
                self.run_qualifier(self.make_core(directory, name="web"))
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(SystemExit, "non-executable"):
                self.run_qualifier(self.make_core(directory, executable=False))

    def test_links_and_open_verifier_parameters_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            product = self.make_core(directory)
            (product / "alias").symlink_to(".ai/manifest.yaml")
            with self.assertRaisesRegex(SystemExit, "symbolic link"):
                self.run_qualifier(product)
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(SystemExit, "caller-authored"):
                self.run_qualifier(self.make_core(directory), {"extra": True})

    def test_build_and_capture_are_core_only_and_closed(self):
        with patch("sys.stdin", io.StringIO(json.dumps({
                "release_input": release_input("web")}))):
            with self.assertRaisesRegex(SystemExit, "initial Core"):
                runpy.run_path(str(BUILD), run_name="__main__")
        request = {
            "release_input": release_input("web"),
            "materialization_result_hash": "d" * 64,
            "signed_tree_manifest_hash": "e" * 64,
            "manifest_item_hash": "f" * 64,
            "signed_manifest": signed_manifest("web"),
        }
        with patch("sys.stdin", io.StringIO(json.dumps(request))):
            with self.assertRaisesRegex(SystemExit, "initial Core"):
                runpy.run_path(str(CAPTURE), run_name="__main__")
        with patch("sys.stdin", io.StringIO(json.dumps({"release_input": release_input(),
                                                          "extra": True}))):
            with self.assertRaisesRegex(SystemExit, "closed"):
                runpy.run_path(str(BUILD), run_name="__main__")


if __name__ == "__main__":
    unittest.main()
