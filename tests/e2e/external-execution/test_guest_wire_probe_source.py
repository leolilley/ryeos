"""Check the source shape of the guest-wire qualification data.

This cannot prove a restored guest, provider contact, or signature validity.
"""

from pathlib import Path
import re
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[3]
CODEX = ROOT / "bundles/codex/.ai"
CLAIM = "external_guest_inputs_v5_launcher_v4"
SUPERVISOR = "ryeos-external-candidate-supervisor"
LAUNCHER = "ryeos-external-candidate-launcher"


def load(path):
    text = path.read_text()
    return yaml.safe_load(text.partition("\n")[2] if text.startswith("# ryeos:signed:") else text)


def policy():
    return load(CODEX / "config/codex/guest-wire-qualification-policy.yaml")[
        "product_qualification_policy"]


class GuestWireProbeSourceTests(unittest.TestCase):
    def test_tool_binds_the_owner_slot_and_the_callback_protocol(self):
        tool = load(CODEX / "tools/codex/qualification/guest-wire-verifier.yaml")
        self.assertEqual(tool["executor_id"], "@subprocess")
        self.assertEqual(tool["execution_protocol"],
                         "protocol:ryeos/core/qualification_consumer_callback")
        self.assertEqual(tool["effects"], "live")
        self.assertEqual(tool["filesystem_authority"], "captured_execution")
        self.assertEqual(tool["required_caps"], [])
        self.assertEqual(tool["external_product_slots"], [{
            "id": "subject",
            "relationship_ref": "config:codex/guest-owner-runtime-products",
            "relationship": "owner_to_guest_wire_verifier",
            "kind": "tree",
            "mount_root": "execution_runtime",
            "mount": "guest-owner-runtime",
        }])
        self.assertEqual(tool["config"]["command"], "bin:codex/ryeos-guest-wire-verifier")
        self.assertEqual(tool["config"]["args"], ["--remote-probe-controller"])
        self.assertEqual(tool["config"]["input_data"], "${params_json}")
        self.assertIn("owner executable digest and manifest hash", tool["description"])
        schema = tool["config_schema"]
        self.assertFalse(schema["additionalProperties"])
        self.assertEqual(set(schema["required"]), set(schema["properties"]))
        self.assertEqual(set(schema["properties"]), {
            "schema", "controller", "claims_on_success",
            "guest_inputs_schema", "launcher_spec_schema",
        })

    def test_policy_is_a_v5_probe_policy_without_a_consumer_context(self):
        p = policy()
        self.assertEqual(p["schema"], "ryeos.product_qualification_policy.v5")
        self.assertEqual(p["verifier_ref"], "tool:codex/qualification/guest-wire-verifier")
        self.assertEqual(p["subject_declaration_id"], "subject")
        self.assertEqual(p["allowed_claims"], [CLAIM])
        self.assertEqual(p["minimum_verifier_process_settlement"], "scope_empty")
        self.assertNotIn("consumer_execution_context", p)
        parameters = p["verifier_parameters"]
        self.assertEqual(parameters["controller"]["scenario_id"], "guest_wire")
        self.assertEqual(parameters["controller"]["maximum_duration_ms"], 240000)
        self.assertEqual(parameters["claims_on_success"], [CLAIM])
        self.assertEqual((parameters["guest_inputs_schema"], parameters["launcher_spec_schema"]), (5, 4))
        scenario = p["producer_scenarios"]["guest_wire"]
        self.assertNotIn("recipe_ref", scenario)
        self.assertNotIn("consumer_worker_request", scenario)
        verifier = scenario["remote_verifier"]
        self.assertEqual(verifier["binary_ref"], "bin:codex/ryeos-guest-wire-verifier")
        self.assertEqual(verifier["guest_target_triple"], "x86_64-unknown-linux-gnu")
        self.assertEqual(verifier["archive_members"], {
            SUPERVISOR: "bin:codex/" + SUPERVISOR,
            LAUNCHER: "bin:codex/" + LAUNCHER,
        })
        self.assertEqual(scenario["remote_probe_input"], {
            "schema": "ryeos.guest_wire_probe_input.v1",
            "guest_inputs_schema": 5,
            "launcher_spec_schema": 4,
        })

    def test_consumer_policy_moves_to_the_same_schema_generation(self):
        consumer = load(CODEX / "config/codex/consumer-qualification-policy.yaml")
        self.assertEqual(consumer["product_qualification_policy"]["schema"],
                         "ryeos.product_qualification_policy.v5")

    def test_relationship_names_the_graph_slot_bounds_and_claim(self):
        products = load(CODEX / "config/codex/guest-owner-runtime-products.yaml")
        relationships = products["product_relationships"]["relationships"]
        self.assertEqual([r["name"] for r in relationships], ["owner_to_guest_wire_verifier"])
        relationship = relationships[0]
        self.assertEqual(relationship["producer"]["canonical_ref"],
                         "graph:codex/guest-owner-runtime-production-for-controller")
        self.assertEqual(relationship["producer"]["recipe_binding"], "product_recipe")
        self.assertEqual(relationship["producer"]["product_name"], "runtime")
        self.assertEqual(set(relationship["producer"]["parameters"]), {"controller_public_key"})
        self.assertRegex(relationship["producer"]["parameters"]["controller_public_key"],
                         r"^ed25519:[A-Za-z0-9+/]{43}=$")
        # The shipped file carries the RFC 8032 placeholder and says it must be
        # replaced per controller before signing; the verifier refuses it.
        self.assertEqual(relationship["producer"]["parameters"]["controller_public_key"],
                         "ed25519:11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=")
        text = (CODEX / "config/codex/guest-owner-runtime-products.yaml").read_text()
        self.assertIn("replaced per hosted", text)
        self.assertIn("controller before signing", text)
        self.assertEqual(relationship["consumer"], {
            "canonical_ref": "tool:codex/qualification/guest-wire-verifier",
            "declaration_id": "subject",
        })
        product = next(item for item in products["build_products"]["products"]
                       if item["name"] == "runtime")
        self.assertEqual(relationship["required_product"], {
            "shape": product["shape"],
            "storage": product["storage"],
            "bounds": product["bounds"],
        })
        self.assertEqual(relationship["qualification"], {
            "policy_ref": "config:codex/guest-wire-qualification-policy",
            "required_claims": [CLAIM],
        })

    def test_companions_are_owned_by_codex_and_shared_with_the_sandbox_bundles(self):
        ownership = load(ROOT / "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml")
        owners = {}
        for bundle in ownership["payload_ownership"]["bundles"]:
            for payload in bundle["payloads"]:
                owners[payload["binary"]] = (bundle["bundle_name"], payload["build_class"])
        for binary in (SUPERVISOR, LAUNCHER, "ryeos-guest-wire-verifier"):
            self.assertEqual(owners[binary], ("codex", "static"), binary)
        shared = {
            entry["binary"]: entry
            for entry in yaml.safe_load(
                (ROOT / "scripts/release/shared-static-payloads.yaml").read_text()
            )["shared_payloads"]
        }
        self.assertEqual(shared[SUPERVISOR]["owner_bundle"], "codex")
        self.assertEqual(sorted(shared[SUPERVISOR]["consumer_bundles"]),
                         ["modal-sandbox", "render-sandbox"])
        self.assertEqual(shared[LAUNCHER]["owner_bundle"], "codex")
        self.assertEqual(shared[LAUNCHER]["consumer_bundles"], ["render-sandbox"])

    def test_populate_script_builds_and_requires_the_new_static_payloads(self):
        script = (ROOT / "scripts/populate-bundles.sh").read_text()
        self.assertIn("ryeos-guest-wire-verifier) build_static_guest_wire_verifier=1", script)
        self.assertIn('require_static_payload "$PAYLOAD_STAGE/codex/ryeos-guest-wire-verifier"', script)
        for binary in (SUPERVISOR, LAUNCHER):
            self.assertIn(f'require_static_payload "$PAYLOAD_STAGE/codex/{binary}"', script)
            self.assertNotIn(f'"$PAYLOAD_STAGE/render-sandbox/{binary}"', script)

    def test_claim_name_is_signed_data_not_mechanism_code(self):
        crate = ROOT / "crates/tools/guest-wire-verifier"
        if not crate.is_dir():
            self.skipTest("verifier crate is not present")
        for path in sorted(crate.rglob("*.rs")):
            text = path.read_text()
            if CLAIM not in text:
                continue
            # Only an inline test module may carry the literal as a fixture.
            code = re.split(r"#\[cfg\(test\)\]", text, maxsplit=1)[0]
            self.assertNotIn(CLAIM, code, str(path))


if __name__ == "__main__":
    unittest.main()
