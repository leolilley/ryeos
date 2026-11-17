"""Source boundary for controller-independent owner production.

This does not qualify a producer host, captured product, or provider snapshot.
"""

from pathlib import Path
import re
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[3]
CODEX = ROOT / "bundles/codex/.ai"


def load(relative):
    return yaml.safe_load((CODEX / relative).read_text())


class GuestOwnerProductionSourceTests(unittest.TestCase):
    def test_production_names_no_controller_and_captures_one_product(self):
        graph = load("graphs/codex/guest-owner-runtime-production.yaml")
        tool = load("tools/codex/guest-runtime/produce-owner.yaml")

        self.assertEqual(graph["product_recipe"],
                         "config:codex/guest-owner-runtime-products")
        self.assertEqual(graph["effects"], "live")
        schema = graph["config"]["config_schema"]
        self.assertEqual(schema["properties"], {})
        self.assertFalse(schema["additionalProperties"])
        self.assertNotIn("required", schema)
        nodes = graph["config"]["nodes"]
        self.assertEqual(graph["config"]["start"], "produce")
        self.assertEqual(set(nodes), {"produce", "done"})
        self.assertEqual(nodes["produce"]["action"]["item_id"],
                         "tool:codex/guest-runtime/produce-owner")
        self.assertEqual(nodes["produce"]["action"]["params"], {})
        expressions = yaml.safe_dump(nodes)
        self.assertNotIn("${config.", expressions)
        self.assertEqual(
            re.findall(r"\$\{inputs\.([A-Za-z_][A-Za-z_0-9]*)", expressions), [])
        self.assertEqual(graph["requires"]["capabilities"]["declared"], [
            "ryeos.execute.tool.codex/guest-runtime/produce-owner",
            "ryeos.execute.config.codex/guest-owner-runtime-products",
        ])

        self.assertEqual(tool["config_schema"]["properties"], {})
        self.assertFalse(tool["config_schema"]["additionalProperties"])
        self.assertEqual(tool["filesystem_authority"], "captured_execution")
        self.assertEqual(tool["network_authority"], "isolated")
        self.assertEqual(tool["external_content"][0]["bundle_binary"],
                         "bin:ryeos-external-guest-occurrence-owner")


if __name__ == "__main__":
    unittest.main()
