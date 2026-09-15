"""Pure checks for the live acceptance assertions; no node or workload contact."""

import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "live_assertions", Path(__file__).with_name("assert-live-response.py")
)
assertions = importlib.util.module_from_spec(spec)
spec.loader.exec_module(assertions)


class AcceptedReplayEqualityTests(unittest.TestCase):
    def setUp(self):
        self.original = {
            "schema": "ryeos.product_build_accepted_result.v1",
            "kind": "product_build_accepted_result",
            "owner_principal": "fp:" + "a" * 64,
            "producer_partition_identity": "b" * 64,
            "products": [{"product_name": "runtime", "witness_hash": "c" * 64}],
        }

    def test_equal_result_in_different_execution_envelopes(self):
        with patch.object(assertions, "load", return_value={"result": self.original}):
            assertions.accepted_equal({"state": {"accepted": self.original}}, "original")

    def test_same_witnesses_do_not_hide_changed_authority(self):
        for field in ("owner_principal", "producer_partition_identity"):
            with self.subTest(field=field):
                replay = dict(self.original, **{field: "changed"})
                with patch.object(assertions, "load", return_value=self.original):
                    with self.assertRaisesRegex(SystemExit, "complete accepted"):
                        assertions.accepted_equal(replay, "original")

    def test_missing_or_ambiguous_result_refuses(self):
        other = dict(self.original, owner_principal="different")
        for replay in ({}, [self.original, other]):
            with self.subTest(replay=replay):
                with patch.object(assertions, "load", return_value=self.original):
                    with self.assertRaisesRegex(SystemExit, "expected one distinct"):
                        assertions.accepted_equal(replay, "original")


if __name__ == "__main__":
    unittest.main()
