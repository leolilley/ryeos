#!/usr/bin/env python3

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
GRAPH = ROOT / "bundles/bundle-release/.ai/graphs/ryeos/bundle-release/publish.yaml"
SERVICES = ROOT / "bundles/bundle-release/.ai/services/bundle-release"


class NativeBundleProducerSurfaceTests(unittest.TestCase):
    def test_graph_has_closed_ordered_release_operations(self):
        body = GRAPH.read_text()
        operations = [
            "input-inspect", "generation-build", "request-tree-signing",
            "generation-capture", "generation-qualify", "generation-finalize",
            "request-authorization", "set-compose", "catalog-request-publication",
        ]
        offsets = [body.index(f"service:bundle-release/{name}") for name in operations]
        self.assertEqual(offsets, sorted(offsets))

    def test_every_graph_operation_has_a_service_contract(self):
        expected = {
            "input-inspect", "generation-build", "request-tree-signing",
            "generation-capture", "generation-qualify", "generation-finalize",
            "request-authorization", "set-compose", "catalog-request-publication",
        }
        available = {path.stem for path in SERVICES.glob("*.yaml")}
        self.assertTrue(expected.issubset(available))
        self.assertTrue({"submit", "status"}.issubset(available))

    def test_catalog_signing_and_catalog_head_publication_are_distinct(self):
        graph = GRAPH.read_text()
        request_offset = graph.index("service:bundle-release/catalog-request-publication")
        publish_offset = graph.index("service:bundle-release/catalog-remote-publish")
        self.assertLess(request_offset, publish_offset)
        handler = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_catalog_publish.rs").read_text()
        self.assertIn("ctx.require_verified()", handler)
        self.assertIn("authenticated_principal: ctx.fingerprint", handler)
        self.assertNotIn("authenticated_principal: req.", handler)
        descriptor = ROOT / "bundles/bundle-source/.ai/services/bundle-catalog/publish.yaml"
        self.assertTrue(descriptor.is_file())
        self.assertIn("expected_catalog_head: string?", descriptor.read_text())

    def test_default_topology_uses_remote_upload_not_local_staging(self):
        graph = GRAPH.read_text()
        profile = (ROOT / "bundles/.ai/node/init/profiles/release-authority.yaml").read_text()
        operations = (ROOT / "deploy/bundle-source-operations.md").read_text()
        self.assertNotIn("service:bundle-catalog/stage-local", graph)
        self.assertIn("service:bundle-release/catalog-remote-publish", graph)
        self.assertNotIn("ryeos.execute.service.bundle-catalog/stage-local", profile)
        self.assertIn("ryeos.execute.service.bundle-release/catalog-remote-publish", profile)
        self.assertNotIn("ryeos.execute.service.bundle-catalog/upload", profile)
        self.assertIn("optional optimization", operations)
        self.assertIn("not used by the default release Graph", operations)

    def test_surface_has_no_legacy_or_generic_signing_escape(self):
        bodies = [GRAPH.read_text()]
        bodies.extend(path.read_text() for path in SERVICES.glob("*.yaml"))
        combined = "\n".join(bodies).lower()
        for forbidden in ["archive", "oci", "legacy", "sign-any", "tool:ryeos/core/sign"]:
            self.assertNotIn(forbidden, combined)
        self.assertIn("ryeos_bundle_sign_v1", combined)

    def test_compiled_handlers_fail_closed_behind_typed_authority_adapter(self):
        handler = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs").read_text()
        owner = (ROOT / "crates/daemon/ryeos-app/src/bundle_publication/producer.rs").read_text()
        self.assertEqual(handler.count("descriptor!("), 5)
        for custom in ["INPUT_INSPECT", "GENERATION_BUILD", "GENERATION_QUALIFY", "GENERATION_FINALIZE", "SET_COMPOSE"]:
            self.assertIn(f"pub const {custom}: ServiceDescriptor", handler)
        self.assertIn("run_authenticated_graph", handler)
        self.assertIn("product_qualification::qualify", handler)
        self.assertNotIn("store_object(&result)", handler)
        self.assertIn("pub const SUBMIT: ServiceDescriptor", handler)
        self.assertIn("dispatch_with_handler_context", handler)
        self.assertIn("begin_external", handler)
        self.assertIn("BundleReleaseAuthorities", handler)
        self.assertIn("has no explicit build/publisher/qualification/catalog authority adapter", handler)
        self.assertNotIn('json!({"status":"ok"', handler.replace(" ", ""))
        for operation in [
            "InputInspect", "GenerationBuild", "RequestTreeSigning", "GenerationCapture",
            "GenerationQualify", "GenerationFinalize", "RequestAuthorization", "SetCompose",
            "CatalogRequestPublication",
        ]:
            self.assertIn(operation, owner)


if __name__ == "__main__":
    unittest.main()
