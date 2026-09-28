#!/usr/bin/env python3

import io
import json
import os
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
GRAPH = ROOT / "bundles/bundle-release/.ai/graphs/ryeos/bundle-release/publish.yaml"
SERVICES = ROOT / "bundles/bundle-release/.ai/services/bundle-release"
NATIVE_QUALIFIER = ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib/native-qualify.py"


class NativeBundleProducerSurfaceTests(unittest.TestCase):
    def test_admitted_cow_source_link_count_is_not_product_link_policy(self):
        tool_dir = ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib"
        for name in ("native-build.py", "core-seed-build.py"):
            body = (tool_dir / name).read_text()
            self.assertNotIn("st_nlink", body, name)
            self.assertIn("stat.S_ISLNK", body, name)
            self.assertIn("shutil.copytree(bundle_root, product", body, name)
        for name in ("native-qualify.py", "core-seed-qualify.py"):
            self.assertIn("st_nlink", (tool_dir / name).read_text(), name)

    def test_native_qualifier_requires_an_executable_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            tree = Path(directory)
            (tree / ".ai/bin").mkdir(parents=True)
            (tree / ".ai/manifest.yaml").write_text(
                '# ryeos:signed:unit-test\n{"name":"native-fixture"}\n',
                encoding="utf-8",
            )
            evidence = io.StringIO()
            source_path = Path

            def realized_path(*parts):
                return tree if parts == ("/ryeos/realizations/native-bundle",) else source_path(*parts)

            environment = {
                "RYEOS_EXTERNAL_REALIZATIONS": json.dumps([
                    {"id": "python", "manifest_hash": "d" * 64},
                    {"id": "subject", "manifest_hash": "c" * 64},
                ]),
                "RYE_THREAD_ID": "native-qualifier-test",
            }
            # Simulated admission only; live node acceptance is separate.
            with patch("pathlib.Path", side_effect=realized_path), patch("sys.stdin", io.StringIO("{}")), patch("sys.stdout", evidence), patch.dict(os.environ, environment):
                with self.assertRaisesRegex(SystemExit, "no executable .ai/bin payload"):
                    runpy.run_path(str(NATIVE_QUALIFIER), run_name="__main__")

            binary = tree / ".ai/bin/native-fixture"
            binary.write_bytes(b"#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
            evidence = io.StringIO()
            with patch("pathlib.Path", side_effect=realized_path), patch("sys.stdin", io.StringIO("{}")), patch("sys.stdout", evidence), patch.dict(os.environ, environment):
                runpy.run_path(str(NATIVE_QUALIFIER), run_name="__main__")
            result = json.loads(evidence.getvalue())
            self.assertEqual(result["probe_evidence"]["binary_count"], 1)
            self.assertIn("native-payloads-executable", result["probe_evidence"]["checks"])

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

    def test_release_authority_enforces_isolation_for_captured_builds(self):
        profile = (
            ROOT / "bundles/.ai/node/init/profiles/release-authority.yaml"
        ).read_text()
        isolation = profile[profile.index("  isolation:") : profile.index("  ingest_ignore:")]
        self.assertIn("      mode: enforce", isolation)
        self.assertIn("        implementation: linux-lillux", isolation)
        self.assertIn("        nested_sandbox: true", isolation)
        self.assertIn("        proc_filesystem: pid_namespace_nested", isolation)

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
