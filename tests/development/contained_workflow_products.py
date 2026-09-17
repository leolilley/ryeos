#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / ".ai/tools/ryeos/development/hosted-oci-runtime/validate-products.py"
SPEC = importlib.util.spec_from_file_location("contained_products", TOOL)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ProductEvidenceTests(unittest.TestCase):
    def request(self):
        return {
            "resolved_config": {
                "schema": "ryeos.development.contained-workflow-products.v1",
                "products": {
                    "image": {"bake_target": "contained-workflow", "docker_target": "ryeos-contained-workflow"},
                    "host_hook": {"bake_target": "contained-oci-hook-artifact", "executable": "ryeos-lillux-oci-hook"},
                    "node_profile": "contained-workflow",
                },
            },
            "evidence": {
                "source_revision": "a" * 40,
                "image_digest": "sha256:" + "b" * 64,
                "hook_sha256": "c" * 64,
                "profile_sha256": "d" * 64,
            },
        }

    def test_exact_products_are_accepted_without_installed_claim(self):
        result = MODULE.evaluate(self.request())
        self.assertTrue(result["accepted"])
        self.assertIn("no installed containment", result["scope"])

    def test_mutable_image_coordinate_refuses(self):
        request = self.request()
        request["evidence"]["image_digest"] = "latest"
        with self.assertRaisesRegex(ValueError, "image digest"):
            MODULE.evaluate(request)

    def test_alternate_product_coordinate_refuses(self):
        request = self.request()
        request["resolved_config"]["products"]["image"]["docker_target"] = "hosted-workflow"
        with self.assertRaisesRegex(ValueError, "product coordinates"):
            MODULE.evaluate(request)

    def test_all_declared_source_inputs_exist(self):
        body = (ROOT / ".ai/config/development/ryeos/contained-workflow-products.yaml").read_text()
        for relative in (
            "Dockerfile.release",
            "docker-bake.release.hcl",
            "bundles/.ai/node/init/profiles/contained-workflow.yaml",
            "images/contained-workflow/entrypoint.sh",
            "crates/tools/lillux-oci-hook",
        ):
            self.assertIn(f"  - {relative}", body)
            self.assertTrue((ROOT / relative).exists(), relative)


if __name__ == "__main__":
    unittest.main()
