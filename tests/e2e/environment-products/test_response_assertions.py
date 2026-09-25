"""Pure checks for the live acceptance assertions; no node or workload contact."""

import importlib.util
import io
from contextlib import redirect_stdout
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


class ComposedSelectionIdentityTests(unittest.TestCase):
    def setUp(self):
        self.snapshot = "a" * 64
        self.distribution_witness = "b" * 64
        self.runtime_witness = "c" * 64
        self.qualification = "d" * 64
        self.manifests = {"distribution": "e" * 64, "runtime": "f" * 64}
        self.bindings = {"distribution": "1" * 64, "runtime": "2" * 64}
        self.response = {
            "project_context": {"snapshot_hash": self.snapshot},
            "consumer": {
                "kind": "pinned_project",
                "consumer_ref": "config:test/runtime-consumer",
                "project_snapshot_hash": self.snapshot,
            },
            "selections": [
                {
                    "declaration_id": "distribution",
                    "witness_hash": self.distribution_witness,
                    "witness_source": {"kind": "local_capture"},
                    "qualification_hash": None,
                },
                {
                    "declaration_id": "runtime",
                    "witness_hash": self.runtime_witness,
                    "witness_source": {"kind": "local_capture"},
                    "qualification_hash": self.qualification,
                },
            ],
            # Shape-only samples: these do not attest product authority or
            # reproduce the product owner's semantic digest computation.
            "selection_identity_digests": {
                "distribution": "3" * 64,
                "runtime": "4" * 64,
            },
            "bindings": [
                {
                    "declaration_ids": [declaration],
                    "manifest_kind": "external_large_content_manifest",
                    "manifest_hash": manifest,
                    "binding": {
                        "consumer_ref": "config:test/runtime-consumer",
                        "manifest_hash": manifest,
                        "binding_hash": self.bindings[declaration],
                    },
                }
                for declaration, manifest in self.manifests.items()
            ],
            "pre_selection_effective_definition_digest": "5" * 64,
            "selected_effective_definition_digest": "6" * 64,
        }

    def check_composed(self):
        output = io.StringIO()
        with redirect_stdout(output):
            assertions.composed(
                {"result": self.response},
                self.snapshot,
                self.distribution_witness,
                self.runtime_witness,
                self.qualification,
                self.manifests["distribution"],
                self.manifests["runtime"],
            )
        return output.getvalue()

    def test_exact_identity_map_preserves_binding_output(self):
        self.assertEqual(
            self.check_composed(),
            self.bindings["distribution"] + "\n" + self.bindings["runtime"] + "\n",
        )

    def test_missing_identity_map_refuses(self):
        del self.response["selection_identity_digests"]
        with self.assertRaisesRegex(SystemExit, "exact selection identity digest map"):
            self.check_composed()

    def test_missing_or_extra_identity_keys_refuse(self):
        valid = self.response["selection_identity_digests"]
        for mapping in (
            {},
            {"distribution": valid["distribution"]},
            {"runtime": valid["runtime"]},
            dict(valid, extra="7" * 64),
        ):
            with self.subTest(mapping=mapping):
                self.response["selection_identity_digests"] = mapping
                with self.assertRaisesRegex(SystemExit, "exact selection identity digest map"):
                    self.check_composed()

    def test_wrong_identity_map_type_refuses(self):
        for mapping in (None, [], ["distribution", "runtime"], "invalid", 1, True):
            with self.subTest(mapping=mapping):
                self.response["selection_identity_digests"] = mapping
                with self.assertRaisesRegex(SystemExit, "exact selection identity digest map"):
                    self.check_composed()

    def test_malformed_identity_digest_refuses_for_each_declaration(self):
        for declaration in ("distribution", "runtime"):
            for digest in (None, "", "a" * 63, "a" * 65, "A" * 64, "g" * 64,
                           "a" * 64 + "\n", 1, True, [], {}):
                with self.subTest(declaration=declaration, digest=digest):
                    self.response["selection_identity_digests"] = {
                        "distribution": "3" * 64,
                        "runtime": "4" * 64,
                        declaration: digest,
                    }
                    with self.assertRaisesRegex(SystemExit, "selection identity.*canonical SHA-256"):
                        self.check_composed()

    def test_valid_identity_map_does_not_bypass_manifest_binding_check(self):
        self.response["bindings"][0]["manifest_hash"] = "0" * 64
        with self.assertRaisesRegex(SystemExit, "wrong distribution manifest"):
            self.check_composed()


if __name__ == "__main__":
    unittest.main()
