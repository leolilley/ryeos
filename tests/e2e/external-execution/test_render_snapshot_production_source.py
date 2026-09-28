"""Check the bounded admitted receipt-to-locator graph source.

This cannot prove provider contact, snapshot availability, or restored bytes.
"""

from pathlib import Path
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[3]
RENDER = ROOT / "bundles/render-sandbox/.ai"


class RenderSnapshotProductionSourceTests(unittest.TestCase):
    def test_received_witness_is_the_only_snapshot_source(self):
        graph = yaml.safe_load(
            (RENDER / "graphs/render-sandbox/owner-snapshot-production.yaml").read_text()
        )
        manifest = yaml.safe_load((RENDER / "manifest.source.yaml").read_text())
        self.assertIn("graph", manifest["requires_kinds"])
        self.assertEqual(graph["effects"], "live")
        self.assertEqual(graph["config"]["on_error"], "fail")
        self.assertEqual(graph["config"]["max_steps"], 3)
        self.assertEqual(graph["config"]["config_schema"]["required"], [
            "remote", "witness_hash", "origin_admission_hash",
            "maximum_bytes", "binding_id", "source_occurrence_id",
        ])
        self.assertFalse(graph["config"]["config_schema"]["additionalProperties"])

        receive = graph["config"]["nodes"]["receive"]
        produce = graph["config"]["nodes"]["produce"]
        self.assertEqual(receive["action"]["item_id"],
                         "service:external-content/receive-product")
        self.assertEqual(receive["next"], {"type": "unconditional", "to": "produce"})
        self.assertEqual(produce["action"]["item_id"],
                         "service:external-content/produce-runtime-snapshot")
        self.assertEqual(produce["action"]["params"]["source"], {
            "kind": "received",
            "acceptance_hash": "${state.receipt.acceptance_hash}",
        })
        self.assertEqual(produce["action"]["params"]["witness_hash"],
                         "${config.witness_hash}")
        self.assertEqual(graph["config"]["nodes"]["done"]["output"], {
            "receipt": "${state.receipt}",
            "snapshot": "${state.snapshot}",
        })
        self.assertEqual(set(graph["requires"]["capabilities"]["declared"]), {
            "ryeos.execute.service.external-content/receive-product",
            "ryeos.execute.service.external-content/produce-runtime-snapshot",
        })


if __name__ == "__main__":
    unittest.main()
