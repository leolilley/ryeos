#!/usr/bin/env python3
"""Non-Cargo acceptance proof for the first independent web bundle release."""

import json
from pathlib import Path
import re
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "tests/fixtures/native-bundle-publication/web-bundle-only-release.json"
GRAPH = ROOT / "bundles/bundle-release/.ai/graphs/ryeos/bundle-release/publish.yaml"
BUILD = ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib/native-build.py"
INSPECTOR = ROOT / "scripts/release/inspect-native-bundle-input.py"
CONSUMER = ROOT / "crates/daemon/ryeos-app/src/bundle_publication/consumer.rs"
RELEASE_HANDLER = ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_release.rs"
PLANNER = ROOT / "crates/daemon/ryeos-bundle/src/plan.rs"
APPLIER = ROOT / "crates/daemon/ryeos-node/src/bundle_set_apply.rs"
ENTRYPOINT = ROOT / "deploy/entrypoint.sh"
SUBSTRATE_WORKFLOW = ROOT / ".github/workflows/publish-ryeos-substrate.yml"
HASH = re.compile(r"^[0-9a-f]{64}$")


class WebBundleOnlyAcceptance(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.case = json.loads(FIXTURE.read_text())

    def test_build_is_exactly_one_owned_web_package(self):
        case = self.case
        self.assertEqual(case["bundle"], "web")
        self.assertEqual(case["cargo_packages"], ["ryeos-web-tools"])
        self.assertEqual(
            case["build_argv"],
            ["cargo", "build", "--release", "--locked", "--target",
             "x86_64-unknown-linux-gnu", "-p", "ryeos-web-tools"],
        )
        plan = json.loads(subprocess.run(
            [str(INSPECTOR), "--repository-root", str(ROOT), "--bundle", "web",
             "--source-snapshot-hash", "b" * 64, "--target", "x86_64-unknown-linux-gnu"],
            text=True, capture_output=True, check=True).stdout)
        self.assertEqual(plan["payloads"], [{"bundle": "web", "binary": "ryeos-web-tools",
            "cargo_package": "ryeos-web-tools", "build_class": "release",
            "bundle_sets": ["central-host", "full", "release-artifacts", "release-authority"]}])
        source = BUILD.read_text()
        self.assertIn('cargo = "/ryeos/realizations/platform/rust/bin/cargo"', source)
        self.assertIn('"build", "--release", "--locked", "--frozen", "--offline"', source)
        self.assertNotIn("--workspace", source)
        self.assertIn('payloads != expected', source)
        self.assertIn('name == "core"', source)
        self.assertIn('"ambient_target_reuse_allowed": False', INSPECTOR.read_text())

    def test_real_inspector_emits_the_exact_web_plan(self):
        result = subprocess.run(
            [
                "python3", str(INSPECTOR), "--repository-root", str(ROOT),
                "--bundle", "web", "--source-snapshot-hash", "b" * 64,
                "--target", "x86_64-unknown-linux-gnu", "--build-profile", "release",
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        plan = json.loads(result.stdout)
        self.assertEqual(plan["bundle_name"], "web")
        self.assertEqual(plan["cargo_packages"], ["ryeos-web-tools"])
        self.assertEqual([payload["binary"] for payload in plan["payloads"]], ["ryeos-web-tools"])
        self.assertTrue(plan["clean_output_required"])
        self.assertFalse(plan["ambient_target_reuse_allowed"])

    def test_normal_release_has_no_image_or_daemon_build(self):
        normal_surface = GRAPH.read_text() + "\n" + BUILD.read_text()
        for forbidden in ["docker build", "buildx", "Dockerfile.release", "ryeosd", "ryeos-substrate"]:
            self.assertNotIn(forbidden, normal_surface)
        workflow = SUBSTRATE_WORKFLOW.read_text()
        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn("Normal bundle publication must not call this workflow", workflow)

    def test_substrate_coordinate_is_unchanged(self):
        before = self.case["substrate_image_before"]
        after = self.case["substrate_image_after"]
        self.assertEqual(before, after)
        self.assertRegex(before, r"^ghcr\.io/leolilley/ryeos-substrate@sha256:[0-9a-f]{64}$")
        entrypoint = ENTRYPOINT.read_text()
        self.assertIn("installed bundle generation absent; seeding from substrate image", entrypoint)
        self.assertIn("preserving installed bundle generation; image seed is first-boot only", entrypoint)

    def test_remote_upload_precedes_catalog_head_publication(self):
        expected = [
            "service:bundle-release/catalog-request-publication",
            "service:bundle-release/catalog-remote-publish",
        ]
        self.assertEqual(self.case["catalog_operations"], expected)
        graph = GRAPH.read_text()
        offsets = [graph.index(operation) for operation in expected]
        self.assertEqual(offsets, sorted(offsets))
        self.assertNotIn("service:bundle-catalog/stage-local", graph)
        upload = (ROOT / "bundles/bundle-source/.ai/services/bundle-catalog/upload.yaml").read_text()
        publish = (ROOT / "bundles/bundle-source/.ai/services/bundle-catalog/publish.yaml").read_text()
        self.assertIn("expected_catalog_head: string?", upload)
        self.assertIn("upload_session_id: string", publish)
        self.assertIn("expected_catalog_head: string?", publish)

    def test_remote_upload_carries_a_bounded_closure_to_an_explicit_source(self):
        graph = GRAPH.read_text()
        remote_offset = graph.index("service:bundle-release/catalog-remote-publish")
        remote_node = graph[remote_offset:]
        self.assertIn("catalog_remote", remote_node)
        handler = RELEASE_HANDLER.read_text()
        body_offset = handler.index("async fn catalog_remote_publish_handler")
        body = handler[body_offset:handler.index("struct ContextualReleaseProof", body_offset)]
        upload_offset = body.index("CATALOG_UPLOAD_SERVICE")
        publish_offset = body.index("CATALOG_PUBLISH_SERVICE", upload_offset)
        self.assertLess(upload_offset, publish_offset)
        self.assertIn("blob_chunks", body[upload_offset:publish_offset])
        self.assertIn("candidate_publication_attestation_hash", body[publish_offset:])
        self.assertIn("upload_session_id", body[publish_offset:])
        catalog_handler = (ROOT / "crates/daemon/ryeos-api/src/handlers/bundle_catalog.rs").read_text()
        self.assertIn("const MAX_UPLOAD_ENTRIES", catalog_handler)
        self.assertIn("const MAX_BLOB_CHUNK_BYTES", catalog_handler)
        self.assertIn("ctx.require_verified()", catalog_handler)
        self.assertIn("catalog.require_uploader(&ctx.fingerprint)?", catalog_handler)

    def test_exact_resolve_fetch_and_apply_are_complete(self):
        coordinate = self.case["resolved_coordinate"]
        self.assertEqual(set(coordinate), {
            "catalog_publication_attestation_hash", "catalog_publication_hash",
            "catalog_snapshot_hash", "set_attestation_hash", "set_hash",
        })
        self.assertTrue(all(HASH.fullmatch(value) for value in coordinate.values()))
        installed, selected = self.case["installed"], self.case["selected"]
        self.assertEqual(set(self.case["fetch"]), set(selected))
        expected_actions = [
            {"bundle": name, "action": "keep" if installed[name] == digest else "replace"}
            for name, digest in sorted(selected.items())
        ]
        self.assertEqual(self.case["apply"], expected_actions)
        consumer = CONSUMER.read_text()
        self.assertIn("pub fn resolve_set_channel", consumer)
        self.assertIn("pub fn plan_fetch", consumer)
        self.assertIn("pub fn plan_exact_set", consumer)
        self.assertIn("complete bundle set omits core", consumer)
        self.assertIn("ReconcileExactSet", PLANNER.read_text())
        applier = APPLIER.read_text()
        self.assertIn("apply_stopped_bundle_set", applier)
        self.assertIn("bundle-set apply requires the daemon to be stopped", applier)


if __name__ == "__main__":
    unittest.main()
