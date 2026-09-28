#!/usr/bin/env python3
"""Static acceptance checks for the purpose-owned consumer update boundary."""
from pathlib import Path
import hashlib
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[2]

class ConsumerUpdateContract(unittest.TestCase):
    def test_exact_stopped_node_coordinator(self):
        source = (ROOT / "crates/daemon/ryeos-node/src/bundle_set_update.rs").read_text()
        for required in (
            "BundleSetUpdateSelection", "expected_catalog_publication_attestation_hash",
            "operator_signing_key", "StoppedBundleSetUpdateAuthority",
            "fn prepare", "apply_stopped_bundle_set", "requires_stopped_node", "admit_journal",
        ):
            self.assertIn(required, source)
        self.assertNotIn("install_single_bundle", source)
        self.assertNotIn("legacy_ref", source)

    def test_request_fixture_is_exact_stopped_node_operation(self):
        path = ROOT / "bundles/bundle-release/.ai/config/bundle-release/stopped-consumer-request.yaml"
        descriptor = yaml.safe_load(path.read_text())
        self.assertEqual(descriptor["operation"], "stopped_exact_set_update")
        self.assertEqual(descriptor["selection"]["mode"], "channel")
        self.assertIn("expected_catalog_publication_attestation_hash", descriptor["selection"])
        self.assertTrue(descriptor["operator_signing_key"].startswith("/"))

    def test_operator_contract_forbids_mutable_and_live_fallbacks(self):
        doc = (ROOT / "bundles/standard/.ai/knowledge/ryeos/standard/bundle-set-update.md").read_text()
        for phrase in ("no mutable-name fallback", "no mutable-name"):
            if phrase in doc.lower():
                break
        else:
            self.fail("documentation must reject mutable-name fallback")
        self.assertIn("whole prospective set", doc)
        self.assertIn("absolute private-key path", doc)
        self.assertIn("Core is substrate-owned and keep-only", doc)

    def test_exact_tree_and_registration_materializers_exist(self):
        tree = (ROOT / "crates/daemon/ryeos-app/src/bundle_publication/tree.rs").read_text()
        registration = (ROOT / "crates/daemon/ryeos-app/src/bundle_transaction.rs").read_text()
        for marker in ("materialize_bundle_tree", "safe_relative", "atomic_create_regular",
                       "inspect_bundle_tree(destination)", "root.sync_tree()"):
            self.assertIn(marker, tree)
        for marker in ("prepare_signed_bundle_registration", "read_regular_file_bounded_no_follow",
                       "sign_content", '"kind": "node"'):
            self.assertIn(marker, registration)

    def test_bootstrap_profiles_authorize_no_unmeasured_catalog(self):
        profiles = ROOT / "bundles/.ai/node/init/profiles"
        checked = 0
        for path in profiles.glob("*.yaml"):
            value = yaml.safe_load(path.read_text())
            section = value.get("policies", {}).get("bundle_publication")
            if section is None:
                continue
            checked += 1
            self.assertEqual(section, {"schema": 2, "catalogs": []}, str(path))
            envelope, body = path.read_bytes().split(b"\n", 1)
            self.assertTrue(envelope.startswith(b"# ryeos:signed:"), str(path))
            self.assertEqual(envelope.split(b":")[-3].decode(), hashlib.sha256(body).hexdigest(), str(path))
        self.assertGreater(checked, 0)

    def test_operational_cli_composes_remote_fetch_and_stopped_apply(self):
        cli = (ROOT / "crates/bin/cli/src/bundle_set_update.rs").read_text()
        lifecycle = (ROOT / "crates/bin/cli/src/lifecycle_commands.rs").read_text()
        remote = (ROOT / "crates/daemon/ryeos-api/src/remote/bundle_set_update.rs").read_text()
        self.assertIn('tokens: &["node", "bundle-set", "update"]', lifecycle)
        self.assertIn("update_stopped_bundle_set", cli)
        for marker in (
            "service:bundle-catalog/resolve",
            "objects_closure_get",
            "verify_exact_coordinate",
            "verify_curated_set",
            "materialize_bundle_tree",
            "prepare_signed_bundle_registration",
            "prepare_bundle_set_init_completion",
            "core is substrate-owned",
        ):
            self.assertIn(marker, remote)

if __name__ == "__main__":
    unittest.main()
