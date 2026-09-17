#!/usr/bin/env python3
"""Focused refusal tests for the authored hosted-OCI evidence verifier."""

import importlib.util
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / ".ai/tools/ryeos/development/hosted-oci-runtime/verify.py"
SPEC = importlib.util.spec_from_file_location("hosted_oci_verify", TOOL)
VERIFY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFY)


def request(claim="installed_qualification"):
    required_capabilities = [
        "exact_lifecycle_identity", "strict_controller_descendant",
        "non_root_controller_transition", "scope_quiescence", "scope_termination",
        "scope_recovery", "detached_descendant_control", "nested_descendant_control",
        "host_observed_cleanup_before_reuse",
    ]
    required_refusals = [
        "missing_delegation", "read_only_delegation", "wrong_delegation_owner",
        "replaced_delegation", "wrong_node_identity", "replaced_app_root",
        "wrong_controller_account", "stale_binding",
    ]
    required_observations = [
        "exact_runtime_coordinates", "physical_c_to_r_containment",
        "non_root_controller", "real_scoped_worker", "detached_descendant_containment",
        "nested_descendant_containment", "freeze_excludes_writers",
        "cancellation_recovery", "daemon_crash_recovery",
        "daemon_restart_preserves_generation", "container_replacement_changes_generation",
        "old_lifetime_dead", "volume_reuse_after_death_only",
        "unrelated_process_untouched", "authenticated_candidate_return",
    ]
    return {
        "resolved_config": {
            "schema": "ryeos.development.hosted-oci-runtime.v1",
            "runtime_product": {
                "image_target": "ryeos-contained-workflow",
                "node_profile": "contained-workflow",
            },
            "claim_classes": ["structural_smoke", "source_contract", "installed_qualification"],
            "required_capabilities": required_capabilities,
            "required_refusals": required_refusals,
            "required_observations": required_observations,
            "limits": {"max_observations": 256},
        },
        "evidence": {
            "schema": "ryeos.hosted-oci-installed-evidence.v1",
            "claim_class": claim,
            "source_revision": "a" * 40,
            "image_digest": "sha256:" + "b" * 64,
            "profile_digest": "sha256:" + "1" * 64,
            "policy_digest": "sha256:" + "c" * 64,
            "node_fingerprint": "d" * 64,
            "binding_digest": "sha256:" + "e" * 64,
            "controller_account": {"implementation": "unix", "uid": 1000, "gid": 1000},
            "lifecycle_identity": {
                "host_boot_id": "exact-boot", "init_pid": 42,
                "init_start_time_ticks": 1234, "scope_identity": {"device": 1, "inode": 2},
            },
            "provider_generation": "sha256:" + "f" * 64,
            "observations": [
                {"id": name, "passed": True, "detail": "fixture"}
                for name in required_observations
            ],
            "capabilities": required_capabilities.copy(),
            "refusals": required_refusals.copy(),
        },
    }


class HostedOciVerifierTests(unittest.TestCase):
    def test_complete_installed_record_is_accepted(self):
        result = VERIFY.evaluate(request())
        self.assertTrue(result["accepted"])
        self.assertEqual([], result["missing_capabilities"])

    def test_installed_record_cannot_omit_physical_containment(self):
        value = request()
        value["evidence"]["capabilities"].remove("strict_controller_descendant")
        result = VERIFY.evaluate(value)
        self.assertFalse(result["accepted"])
        self.assertEqual(["strict_controller_descendant"], result["missing_capabilities"])

    def test_failed_observation_refuses_installed_claim(self):
        value = request()
        value["evidence"]["observations"][0]["passed"] = False
        self.assertFalse(VERIFY.evaluate(value)["accepted"])

    def test_structural_smoke_is_not_promoted_to_installed(self):
        value = request("structural_smoke")
        value["evidence"]["capabilities"] = []
        result = VERIFY.evaluate(value)
        self.assertTrue(result["accepted"])
        self.assertEqual("structural_smoke", result["claim_class"])

    def test_open_or_oversized_records_refuse(self):
        value = request()
        value["evidence"]["invented"] = True
        with self.assertRaisesRegex(ValueError, "closed current record"):
            VERIFY.evaluate(value)
        value = request()
        value["evidence"]["observations"] = [
            {"id": f"extra-{index}", "passed": True, "detail": "fixture"}
            for index in range(257)
        ]
        with self.assertRaisesRegex(ValueError, "exceed"):
            VERIFY.evaluate(value)


if __name__ == "__main__":
    unittest.main()
