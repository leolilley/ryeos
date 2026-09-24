#!/usr/bin/env python3
"""Static contract checks for the pre-policy authority calibration surface."""

from pathlib import Path
import unittest
import yaml


ROOT = Path(__file__).resolve().parents[2]


class AuthorityCalibrationAssets(unittest.TestCase):
    def test_calibration_surface_is_operator_bounded_and_catalog_free(self) -> None:
        handler = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs").read_text()
        contract = (ROOT / "crates/daemon/ryeos-app/src/bundle_publication/calibration.rs").read_text()
        graph = (ROOT / "bundles/bundle-release/.ai/graphs/ryeos/bundle-release/authority-calibrate.yaml").read_text()
        service = (ROOT / "bundles/bundle-release/.ai/services/bundle-release/authority-calibrate.yaml").read_text()
        profile = (ROOT / "bundles/.ai/node/init/profiles/release-authority.yaml").read_text()

        self.assertIn("require_local_configured_operator", handler)
        self.assertIn("run_authority_calibration", handler)
        self.assertNotIn("AuthorityCalibrationRunner>()", handler)
        self.assertIn("AUTHORITY_CALIBRATION_CLAIM", contract)
        self.assertIn("bundle_publication_authority_calibration_v2", contract)
        self.assertIn("source_snapshot_hash", graph)
        self.assertIn("required: [project_path, source_snapshot_hash, execution_environment]", graph)
        self.assertIn('execution_environment: "${inputs.execution_environment}"', graph)
        self.assertIn("execution_environment: object", service)
        for capability in (
            "ryeos.execute.config.bundle-release/execution-environment-products",
            "ryeos.execute.config.bundle-release/native-build-products",
            "ryeos.execute.config.bundle-release/native-qualification",
            "ryeos.execute.config.bundle-release/portable-build-products",
            "ryeos.execute.config.bundle-release/portable-qualification",
            "ryeos.execute.config.bundle-release/portable-signed-capture-products",
            "ryeos.execute.tool.ryeos/bundle-release/portable-qualify",
        ):
            self.assertIn(capability, profile)
        self.assertNotIn("catalog_namespace", graph)
        self.assertNotIn("publisher_fingerprint:", graph)
        self.assertNotIn("catalog_namespace", service)
        self.assertNotIn("publisher_fingerprint:", service)

        for name in (
            "calibration-native-build-products.yaml",
            "calibration-native-capture-products.yaml",
            "calibration-core-build-products.yaml",
            "calibration-core-capture-products.yaml",
            "calibration-substrate-build-products.yaml",
        ):
            recipe = (ROOT / "bundles/bundle-release/.ai/config/bundle-release" / name).read_text()
            self.assertIn("recipe_purpose: authority_calibration_v1", recipe)
            self.assertNotIn("catalog_namespace", recipe)
            self.assertNotIn("publisher", recipe.lower())

        self.assertIn("CalibrationCoreManifestAuthority", handler)
        self.assertIn("accept_dispatch_products", handler)
        self.assertIn("publish_qualification", handler)
        self.assertIn("load_verified_substrate_identity", handler)

    def test_measurement_requires_authenticated_calibration_coordinates(self) -> None:
        handler = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs").read_text()
        service = (ROOT / "bundles/bundle-release/.ai/services/bundle-release/authority-measure.yaml").read_text()
        self.assertIn("calibration_run_attestation_hash", handler)
        self.assertIn("calibration_attestation.verify_with_key", handler)
        self.assertIn("coordinates differ from the authenticated calibration run", handler)
        self.assertIn("expected_calibration_recipes", handler)
        self.assertIn("observe_publisher_tool", handler)
        self.assertIn("require_authority_calibration", handler)
        self.assertIn("calibration_run_attestation_hash", service)
        self.assertIn("project_path: string", service)
        self.assertIn("publisher_executable_path: string", service)
        for field in (
            "portable_qualification_owner_principal",
            "portable_qualification_attestation_hash",
            "native_qualification_owner_principal",
            "native_qualification_attestation_hash",
        ):
            self.assertIn(f"{field}: string", service)
        self.assertNotIn("\n  qualification_owner_principal: string", service)

    def test_two_lane_caps_are_declared_at_every_dispatch_boundary(self) -> None:
        bundle = ROOT / "bundles/bundle-release/.ai"
        native_caps = {
            "graphs/ryeos/bundle-release/authority-calibrate.yaml": ["native-build-products", "native-qualification"],
            "services/bundle-release/authority-calibrate.yaml": ["native-build-products", "native-qualification"],
            "graphs/ryeos/bundle-release/publish.yaml": ["native-qualification"],
            "services/bundle-release/generation-qualify.yaml": ["native-qualification"],
        }
        for relative, names in native_caps.items():
            with self.subTest(asset=relative):
                body = (bundle / relative).read_text()
                for name in names:
                    self.assertIn(f"ryeos.execute.config.bundle-release/{name}", body)
                descriptor = yaml.safe_load(body)
                caps = (
                    descriptor["requires"]["capabilities"]["declared"]
                    if relative.startswith("graphs/")
                    else descriptor["required_caps"]
                )
                self.assertEqual(caps, sorted(set(caps)))
        profiles = ROOT / "bundles/.ai/node/init/profiles"
        for path in profiles.glob("*.yaml"):
            with self.subTest(profile=path.name):
                policy = yaml.safe_load(path.read_text())["policies"]["bundle_publication"]
                self.assertEqual(policy["schema"], 2)
        profile = yaml.safe_load((profiles / "release-authority.yaml").read_text())
        caps = profile["policies"]["command_registration"]["bundle_source_caps"]["bundle-release"]
        self.assertIn("ryeos.execute.config.bundle-release/native-qualification", caps)
        self.assertEqual(caps, sorted(set(caps)))

    def test_calibration_build_capture_consumers_match_portable_and_native_surfaces(self) -> None:
        bundle = ROOT / "bundles/bundle-release/.ai"
        configs = bundle / "config/bundle-release"
        for lane, capture in (("portable", "portable-signed-capture"), ("native", "signed-capture")):
            with self.subTest(lane=lane):
                calibration = yaml.safe_load(
                    (configs / f"calibration-{lane}-build-products.yaml").read_text()
                )
                release = yaml.safe_load((configs / f"{lane}-build-products.yaml").read_text())
                relationships = {
                    relation["name"]: relation
                    for relation in calibration["product_relationships"]["relationships"]
                }
                release_relationships = {
                    relation["name"]: relation
                    for relation in release["product_relationships"]["relationships"]
                }
                expected_names = {
                    f"{lane}_bundle_to_signed_capture",
                    f"{lane}_bundle_to_signed_capture_tool",
                }
                self.assertEqual(set(relationships), expected_names)
                self.assertEqual(set(release_relationships), expected_names)
                self.assertEqual(relationships, release_relationships)

                graph_ref = f"graph:ryeos/bundle-release/{capture}"
                tool_ref = f"tool:ryeos/bundle-release/{capture}"
                graph = yaml.safe_load(
                    (bundle / f"graphs/ryeos/bundle-release/{capture}.yaml").read_text()
                )
                tool = yaml.safe_load(
                    (bundle / f"tools/ryeos/bundle-release/{capture}.yaml").read_text()
                )
                self.assertEqual(graph["config"]["nodes"]["capture"]["action"]["item_id"], tool_ref)
                self.assertEqual(tool["name"], capture)
                for suffix, consumer_ref, descriptor in (
                    ("", graph_ref, graph),
                    ("_tool", tool_ref, tool),
                ):
                    relationship_name = f"{lane}_bundle_to_signed_capture{suffix}"
                    self.assertEqual(
                        relationships[relationship_name]["consumer"],
                        {"canonical_ref": consumer_ref, "declaration_id": "unsigned_bundle"},
                    )
                    self.assertEqual(
                        release_relationships[relationship_name]["consumer"],
                        {"canonical_ref": consumer_ref, "declaration_id": "unsigned_bundle"},
                    )
                    slots = {
                        slot["id"]: slot for slot in descriptor["external_product_slots"]
                    }
                    self.assertEqual(
                        slots["unsigned_bundle"]["relationship"], relationship_name
                    )
                    self.assertEqual(
                        slots["unsigned_bundle"]["relationship_ref"],
                        f"config:bundle-release/{lane}-build-products",
                    )

    def test_release_catalog_admission_remains_intact(self) -> None:
        handler = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs").read_text()
        self.assertGreaterEqual(handler.count("require_catalog("), 5)


if __name__ == "__main__":
    unittest.main()
