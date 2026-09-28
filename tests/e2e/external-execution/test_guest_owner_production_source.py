"""Source boundary for node-rooted and explicitly targeted owner production.

This does not qualify a producer host, captured product, or Render snapshot.
"""

from pathlib import Path
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[3]
CODEX = ROOT / "bundles/codex/.ai"


def load(relative):
    return yaml.safe_load((CODEX / relative).read_text())


class GuestOwnerProductionSourceTests(unittest.TestCase):
    def test_target_controller_is_explicit_and_uses_the_same_captured_product(self):
        local = load("graphs/codex/guest-owner-runtime-production.yaml")
        targeted = load(
            "graphs/codex/guest-owner-runtime-production-for-controller.yaml"
        )
        tool = load("tools/codex/guest-runtime/produce-owner.yaml")

        self.assertEqual(targeted["product_recipe"], local["product_recipe"])
        self.assertEqual(targeted["effects"], "live")
        schema = targeted["config"]["config_schema"]
        self.assertEqual(schema["required"], ["controller_public_key"])
        self.assertFalse(schema["additionalProperties"])
        nodes = targeted["config"]["nodes"]
        self.assertEqual(targeted["config"]["start"], "produce")
        self.assertEqual(nodes["produce"]["action"]["item_id"],
                         "tool:codex/guest-runtime/produce-owner")
        self.assertEqual(nodes["produce"]["action"]["params"], {
            "controller_public_key": "${config.controller_public_key}"
        })
        self.assertNotIn("identity", nodes)
        self.assertEqual(tool["filesystem_authority"], "captured_execution")
        self.assertEqual(tool["network_authority"], "isolated")
        self.assertEqual(tool["external_content"][0]["bundle_binary"],
                         "bin:ryeos-external-guest-occurrence-owner")


if __name__ == "__main__":
    unittest.main()
