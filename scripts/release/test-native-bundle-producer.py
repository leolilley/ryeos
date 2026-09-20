#!/usr/bin/env python3

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
GRAPH = ROOT / "bundles/bundle-release/.ai/graphs/ryeos/bundle-release/publish.yaml"
SERVICES = ROOT / "bundles/bundle-release/.ai/services/bundle-release"


class NativeBundleProducerSurfaceTests(unittest.TestCase):
    def test_tool_invocation_schemas_are_inventory_not_runtime_blocks(self):
        tool_kind = (
            ROOT / "bundles/core/.ai/node/engine/kinds/tool/tool.kind-schema.yaml"
        ).read_text()
        ignored = tool_kind[tool_kind.index("  ignored_keys:") :]
        self.assertIn("    - input_schema", ignored)
        self.assertIn("    - parameters", ignored)
        for name in ("native-build", "core-seed-build"):
            tool = (
                ROOT
                / "bundles/bundle-release/.ai/tools/ryeos/bundle-release"
                / f"{name}.yaml"
            ).read_text()
            self.assertIn("input_schema:", tool)

    def test_catalog_transport_does_not_require_publisher_private_custody(self):
        policy = (ROOT / "crates/daemon/ryeos-app/src/node_policy/sections/bundle_publication.rs").read_text()
        catalog = (ROOT / "crates/daemon/ryeos-app/src/bundle_publication/catalog.rs").read_text()
        upload = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_catalog.rs").read_text()
        release = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs").read_text()
        self.assertIn("pub authorized_uploaders: Vec<String>", policy)
        self.assertIn("catalog.require_uploader(&ctx.fingerprint)?", upload)
        self.assertIn("catalog.require_uploader(state.identity.fingerprint())?", release)
        self.assertIn("catalog_policy.require_uploader(&request.authenticated_principal)?", catalog)
        self.assertIn("authenticated_principal: Some(request.authenticated_principal.clone())", catalog)
        self.assertIn("attestation.verify_with_key(key)?", catalog)
        self.assertIn("attestation.issuer_fingerprint()? != fingerprint", catalog)

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
        execution = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release_execution.rs").read_text()
        implementation = handler + "\n" + execution
        owner = (ROOT / "crates/daemon/ryeos-app/src/bundle_publication/producer.rs").read_text()
        self.assertEqual(handler.count("descriptor!("), 5)
        for custom in ["INPUT_INSPECT", "GENERATION_BUILD", "GENERATION_CAPTURE", "GENERATION_QUALIFY", "GENERATION_FINALIZE", "SET_COMPOSE"]:
            self.assertIn(f"pub const {custom}: ServiceDescriptor", handler)
        self.assertIn("execute_pinned_graph", implementation)
        self.assertIn("product_qualification::qualify", implementation)
        self.assertNotIn("store_object(&result)", implementation)
        self.assertIn("pub const SUBMIT: ServiceDescriptor", handler)
        self.assertIn("dispatch_with_handler_context", implementation)
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
