#!/usr/bin/env python3
"""Static contract checks for the pre-policy authority calibration surface."""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]


class AuthorityCalibrationAssets(unittest.TestCase):
    def test_calibration_surface_is_operator_bounded_and_catalog_free(self) -> None:
        handler = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs").read_text()
        contract = (ROOT / "crates/daemon/ryeos-app/src/bundle_publication/calibration.rs").read_text()
        graph = (ROOT / "bundles/bundle-release/.ai/graphs/ryeos/bundle-release/authority-calibrate.yaml").read_text()
        service = (ROOT / "bundles/bundle-release/.ai/services/bundle-release/authority-calibrate.yaml").read_text()

        self.assertIn("require_local_configured_operator", handler)
        self.assertIn("run_authority_calibration", handler)
        self.assertNotIn("AuthorityCalibrationRunner>()", handler)
        self.assertIn("AUTHORITY_CALIBRATION_CLAIM", contract)
        self.assertIn("bundle_publication_authority_calibration_v1", contract)
        self.assertIn("source_snapshot_hash", graph)
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

    def test_release_catalog_admission_remains_intact(self) -> None:
        handler = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs").read_text()
        self.assertGreaterEqual(handler.count("require_catalog("), 5)


if __name__ == "__main__":
    unittest.main()
