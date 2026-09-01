#!/usr/bin/env python3
"""Bundle-owned conformance tests for the local-inference fixture."""

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import re
import stat
import tempfile
import unittest

import yaml


BUNDLE = Path(__file__).resolve().parent
REPOSITORY = BUNDLE.parent.parent
ACTIVATION_PATH = (
    BUNDLE / ".ai/config/ryeos-runtime/local-tinygrad-activation.yaml"
)
WORKER_PATH = BUNDLE / ".ai/workers/local-inference/local-tinygrad.yaml"
WORKER_SOURCE = BUNDLE / ".ai/workers/local-inference/lib/local-tinygrad"
RELEASE_PATH = (
    REPOSITORY / "scripts/release/local-inference-qwen3-0.6b-v1.json"
)
WORKFLOW_PATH = (
    REPOSITORY / ".github/workflows/publish-local-inference-realizations.yml"
)
SESSION_PROTOCOL_TEST_PATH = (
    BUNDLE / "tests/tinygrad_qwen/test_session_protocol.py"
)
RYEOS_RELEASE_WORKFLOW_PATH = REPOSITORY / ".github/workflows/publish-ryeosd.yml"
SOURCE_QUALIFIER_WORKFLOW_PATH = (
    REPOSITORY / ".github/workflows/qualify-local-inference-source.yml"
)
AUTHOR_PATH = REPOSITORY / "scripts/release/author-local-inference-realizations.py"
NODE_QUALIFIER_PATH = REPOSITORY / "scripts/release/qualify-local-inference-node.sh"
RELEASE_VERIFIER_PATH = (
    REPOSITORY / "scripts/release/verify-local-inference-release.py"
)
ACTIVATION_KNOWLEDGE_PATH = (
    BUNDLE / ".ai/knowledge/local-inference/activation.md"
)


def worker_source_manifest_digest() -> str:
    entries = []
    total = 0
    for path in sorted(
        (candidate for candidate in WORKER_SOURCE.rglob("*") if candidate.is_file()),
        key=lambda candidate: candidate.relative_to(WORKER_SOURCE).as_posix().encode(),
    ):
        content = path.read_bytes()
        total += len(content)
        mode = path.stat().st_mode
        entries.append(
            {
                "root": "source",
                "path": path.relative_to(WORKER_SOURCE).as_posix(),
                "blob_hash": hashlib.sha256(content).hexdigest(),
                "size": len(content),
                "mode": "executable"
                if mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
                else "read_only",
            }
        )
    manifest = {
        "schema": 1,
        "kind": "ryeos.source_closure_manifest",
        "roots": [{"id": "source"}],
        "entries": entries,
        "totals": {"file_count": len(entries), "total_bytes": total},
    }
    canonical = json.dumps(
        manifest, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode()
    return hashlib.sha256(canonical).hexdigest()


class LocalInferenceContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.activation = yaml.safe_load(
            ACTIVATION_PATH.read_text(encoding="utf-8")
        )
        cls.worker = yaml.safe_load(WORKER_PATH.read_text(encoding="utf-8"))
        cls.release = json.loads(RELEASE_PATH.read_text(encoding="utf-8"))

    def test_activation_is_a_closed_whole_tree_recipe(self) -> None:
        self.assertEqual(
            self.activation["schema"],
            "ryeos.external_content_activation.v3",
        )
        self.assertEqual(
            self.activation["consumer_ref"],
            "worker:local-inference/local-tinygrad",
        )
        self.assertNotIn("persistent_session_policy", self.activation)
        self.assertEqual(len(self.activation["sources"]), 4)
        self.assertEqual(len(self.activation["components"]), 4)
        self.assertTrue(
            all(not source.get("members") for source in self.activation["sources"])
        )
        self.assertTrue(
            all(
                component["shape"]["kind"] == "whole_archive_tree"
                for component in self.activation["components"]
            )
        )
        serialized = ACTIVATION_PATH.read_text(encoding="utf-8")
        for forbidden in (
            "command:",
            "transform:",
            "named_root",
            "assembly",
            "manifest_schema",
            "expected_manifest_hash",
        ):
            self.assertNotIn(forbidden, serialized)

    def test_release_pins_and_consumer_manifest_authority_agree(self) -> None:
        releases = {
            realization["component"]: realization
            for realization in self.release["realizations"]
        }
        sources = {
            source["id"].removesuffix("_archive"): source
            for source in self.activation["sources"]
        }
        components = {
            component["id"]: component
            for component in self.activation["components"]
        }
        declarations = {
            declaration["id"]: declaration
            for declaration in self.worker["external_content"]
        }
        self.assertEqual(set(releases), set(sources))
        self.assertEqual(set(releases), set(components))
        self.assertEqual(set(releases), set(declarations))

        for component_id, release in releases.items():
            source = sources[component_id]
            component = components[component_id]
            shape = component["shape"]
            self.assertEqual(source["url"], release["url"])
            self.assertEqual(source["sha256"], release["sha256"])
            self.assertEqual(
                source["maximum_compressed_bytes"],
                release["maximum_compressed_bytes"],
            )
            self.assertEqual(
                source["maximum_expanded_bytes"],
                release["maximum_expanded_bytes"],
            )
            self.assertEqual(
                source["maximum_entries"], release["maximum_entries"]
            )
            self.assertEqual(component["storage"], release["storage"])
            self.assertEqual(shape["source"], f"{component_id}_archive")
            self.assertEqual(shape["prefix"], release["prefix"])
            self.assertEqual(shape["bounds"], release["bounds"])
            self.assertEqual(
                declarations[component_id]["digest"],
                release["manifest_hash"],
            )
            self.assertEqual(declarations[component_id]["kind"], "tree")
            self.assertEqual(declarations[component_id]["mode"], "pinned")

    def test_release_contract_has_exact_aggregate_node_ceilings(self) -> None:
        realizations = self.release["realizations"]
        self.assertEqual(
            sum(item["maximum_compressed_bytes"] for item in realizations),
            1_329_438_282,
        )
        self.assertEqual(
            sum(item["maximum_expanded_bytes"] for item in realizations),
            1_893_273_600,
        )
        self.assertEqual(
            sum(item["maximum_entries"] for item in realizations),
            4_836,
        )

    def test_signed_node_policy_example_covers_the_exact_release(self) -> None:
        knowledge = ACTIVATION_KNOWLEDGE_PATH.read_text(encoding="utf-8")
        match = re.search(
            r"## Node policy\n.*?~~~yaml\n(?P<policy>.*?)\n~~~",
            knowledge,
            flags=re.DOTALL,
        )
        self.assertIsNotNone(match, "signed activation knowledge has no node policy")
        policy = yaml.safe_load(match.group("policy"))
        realizations = self.release["realizations"]
        bounds = [item["bounds"] for item in realizations]
        self.assertEqual(
            policy["limits"]["max_depth"],
            max(item["maximum_depth"] for item in bounds),
        )
        self.assertEqual(
            policy["limits"]["max_entries"],
            max(item["maximum_entries"] for item in bounds),
        )
        self.assertEqual(
            policy["limits"]["max_file_bytes"],
            max(item["maximum_file_bytes"] for item in bounds),
        )
        self.assertEqual(
            policy["limits"]["max_total_bytes"],
            max(item["maximum_total_bytes"] for item in bounds),
        )
        managed = policy["managed_activation"]
        self.assertEqual(managed["max_archives"], len(realizations))
        self.assertEqual(
            managed["max_compressed_bytes"],
            sum(item["maximum_compressed_bytes"] for item in realizations),
        )
        self.assertEqual(
            managed["max_expanded_bytes"],
            sum(item["maximum_expanded_bytes"] for item in realizations),
        )
        self.assertEqual(
            managed["max_members"],
            sum(item["maximum_entries"] for item in realizations),
        )
        self.assertEqual(
            managed["max_member_bytes"],
            max(item["maximum_file_bytes"] for item in bounds),
        )
        self.assertEqual(managed["max_concurrent_activations"], 1)

    def test_worker_source_digest_covers_the_complete_source_closure(self) -> None:
        source = WORKER_PATH.read_text(encoding="utf-8")
        match = re.search(r'(?m)^  digest: "([0-9a-f]{64})"$', source)
        self.assertIsNotNone(match, "worker source digest is absent")
        self.assertEqual(match.group(1), worker_source_manifest_digest())

    def test_worker_declares_its_shared_real_uid_process_ceiling(self) -> None:
        self.assertEqual(
            self.worker["session_resources"],
            {"real_uid_process_limit": 4096},
        )

    def test_release_qualification_starts_the_real_duplex_session(self) -> None:
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        source_qualifier = SOURCE_QUALIFIER_WORKFLOW_PATH.read_text(encoding="utf-8")
        self.assertTrue(SESSION_PROTOCOL_TEST_PATH.is_file())
        self.assertIn(
            "python3 bundles/local-inference/tests/tinygrad_qwen/test_session_protocol.py",
            workflow,
        )
        self.assertIn(
            "scripts/release/qualify-local-inference-node.sh",
            source_qualifier,
        )
        self.assertIn("--bundle-set full", source_qualifier)

        qualifier = NODE_QUALIFIER_PATH.read_text(encoding="utf-8")
        self.assertIn("activation_args+=(online)", qualifier)
        self.assertIn("activation_args+=(offline local-inference-archives)", qualifier)
        self.assertIn("--online", source_qualifier)
        self.assertIn("--archive-root", source_qualifier)
        self.assertEqual(source_qualifier.count("--minimum-free-bytes 2147483648"), 2)
        self.assertIn("--archive-root", qualifier)
        self.assertIn('"minimum_free_bytes": int(sys.argv[2])', qualifier)
        self.assertIn('source "$repository_root/scripts/pkg/bundle-sets.sh"', qualifier)
        self.assertIn("ryeos_bundle_set_names full", qualifier)
        self.assertIn('--source "$qualification_source"', qualifier)
        self.assertNotIn('--source "$bundle_source"', qualifier)
        self.assertNotIn("shutil.copyfile", qualifier)
        self.assertIn(
            'python3 - "$qualification_root" "$minimum_free_bytes" <<\'PY\'',
            qualifier,
        )
        self.assertIn('max_total_address_space_bytes"] = 1', qualifier)
        self.assertIn("provider_attempt_reservation", qualifier)
        self.assertIn("namespace='provider.call'", qualifier)
        self.assertIn("provider_call_observation_recorded", qualifier)
        self.assertIn("directive:qualification/live_tool_loop", qualifier)
        self.assertIn("graph:qualification/live_tool_follow", qualifier)
        self.assertIn("--pin-project --retain-child-results", qualifier)
        self.assertIn("tool:qualification/read", qualifier)
        self.assertIn("tool:qualification/mutate", qualifier)
        self.assertIn("tool:qualification/verify", qualifier)
        self.assertGreaterEqual(
            qualifier.count("- ryeos.execute.tool.qualification/read"), 2,
            "both the live directive and its follow parent must hold read authority",
        )
        self.assertIn(
            '"cancellation_mode": "graceful"',
            qualifier,
            "zero-argument Python tools must distinguish runtime-injected controls",
        )
        self.assertIn("tool_concurrency: 1", qualifier)
        self.assertIn('category: "ryeos-runtime"', qualifier)
        self.assertIn("config:ryeos-runtime/execution", qualifier)
        self.assertIn(
            'expected = ["qualification_read", "qualification_mutate", "qualification_verify"]',
            qualifier,
        )
        self.assertIn("service:threads/tail", qualifier)
        self.assertNotIn("/state/runtime.sqlite3", qualifier)

        publish_position = workflow.index(
            "Publish exact immutable prerelease"
        )
        qualification_position = workflow.index(
            "Qualify exact tag source against the public prerelease"
        )
        promotion_position = workflow.index(
            "Promote the independently qualified immutable release"
        )
        self.assertLess(publish_position, qualification_position)
        self.assertLess(qualification_position, promotion_position)
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn("contents: read", source_qualifier)
        self.assertNotIn("contents: write", source_qualifier)

    def test_toolchain_embeds_the_exact_llvm_notice_closure(self) -> None:
        author = AUTHOR_PATH.read_text(encoding="utf-8")
        expected = {
            "llvm-project-20.1.8-LICENSE.TXT": "8d85c1057d742e597985c7d4e6320b015a9139385cff4cbae06ffc0ebe89afee",
            "llvm-project-20.1.8-ConvertUTF.cpp": "d425e131c4c1e59ad19139ba7bdbebb2cb78cd5253b568b0359001bf08a8a25e",
            "llvm-project-20.1.8-UnicodeNameToCodepointGenerated.cpp": "cf183ee415e1b249b0a4f1755b5a11a95d94c7f723010667ca0f6e4964369be7",
            "llvm-project-20.1.8-xxhash.cpp": "b47e89a65e40f34c7e336a58f1902c958b7bd90b3370bd497c8cb788eb40c2d4",
            "llvm-project-20.1.8-COPYRIGHT.regex": "0424e57d4303164dc59a8509c20dae0518b853692e5c2b0e98b11816fdbc97c7",
            "llvm-project-20.1.8-BLAKE3-LICENSE": "6a94bedb8b707ed97f6e310d0d015ab14e0683ffa0a612b02958581b9cc9fc0e",
            "llvm-project-20.1.8-MD5.cpp": "44256f3d849f65a77140514d87474a00f03322038a40f14c71918b29481977a4",
            "llvm-project-20.1.8-SHA1.cpp": "cc6c4b80b5c2a85f915fd336b72a87aeac696a03c30ce87756e71b060c5ca8a9",
            "llvm-project-20.1.8-SHA256.cpp": "9b1f22d8181e5776527fe8d45948dc31d99d264a68065f6da6d8fab0db7ea232",
        }
        for name, digest in expected.items():
            self.assertIn(name, author)
            self.assertIn(digest, author)

    def test_release_workflow_invokes_the_author_without_shell_artifacts(self) -> None:
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        invocation = (
            "python3 scripts/release/author-local-inference-realizations.py \\\n"
            "            --cache \"$CACHE_DIR\" \\\n"
            "            --output \"$ARTIFACT_DIR\""
        )
        self.assertIn(invocation, workflow)
        self.assertNotIn(".py +", workflow)

    def test_corresponding_sources_and_main_release_gate_are_closed(self) -> None:
        groups = self.release["corresponding_sources"]
        self.assertEqual(
            [group["packages"] for group in groups],
            [
                ["libgcc-14.2.0-r6", "libstdc++-14.2.0-r6"],
                ["xz-libs-5.8.3-r0"],
            ],
        )
        archives = []
        for group in groups:
            self.assertRegex(group["packaging_commit"], r"^[0-9a-f]{40}$")
            for role in ("upstream", "packaging"):
                artifact = group[role]
                self.assertRegex(artifact["sha256"], r"^[0-9a-f]{64}$")
                self.assertTrue(artifact["origin_url"].startswith("https://"))
                self.assertEqual(
                    artifact["url"],
                    "https://github.com/leolilley/ryeos/releases/download/"
                    + self.release["release_tag"]
                    + "/"
                    + artifact["archive"],
                )
                archives.append(artifact["archive"])
        self.assertEqual(len(archives), len(set(archives)))
        release_workflow = RYEOS_RELEASE_WORKFLOW_PATH.read_text(encoding="utf-8")
        source_qualifier = SOURCE_QUALIFIER_WORKFLOW_PATH.read_text(encoding="utf-8")
        verifier = RELEASE_VERIFIER_PATH.read_text(encoding="utf-8")
        self.assertIn("Qualify local inference against this release source", release_workflow)
        self.assertIn("uses: ./.github/workflows/qualify-local-inference-source.yml", release_workflow)
        self.assertIn("source_sha: ${{ needs.qualify.outputs.source_sha }}", release_workflow)
        self.assertIn("require_promoted_release: true", release_workflow)
        self.assertIn("actions: read", release_workflow)
        self.assertIn("publish-local-inference-realizations.yml/runs", source_qualifier)
        self.assertIn('run.get("head_sha") == source_sha', source_qualifier)
        self.assertIn('asset.get("digest") != f"sha256:{digest}"', verifier)
        self.assertNotIn(
            "realization tag source $REALIZATION_SOURCE_SHA differs from release source",
            release_workflow,
        )

    def test_release_metadata_verifier_covers_every_exact_asset(self) -> None:
        spec = importlib.util.spec_from_file_location(
            "verify_local_inference_release",
            RELEASE_VERIFIER_PATH,
        )
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader)
        verifier = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(verifier)

        expected, release_tag = verifier.expected_assets(RELEASE_PATH)
        release = {
            "tag_name": release_tag,
            "draft": False,
            "prerelease": False,
            "assets": [
                {
                    "name": name,
                    "state": "uploaded",
                    "digest": "sha256:" + digest,
                }
                for name, digest in sorted(expected.items())
            ],
        }
        self.assertEqual(len(release["assets"]), 17)
        with tempfile.TemporaryDirectory() as directory:
            release_path = Path(directory) / "release.json"
            release_path.write_text(json.dumps(release), encoding="utf-8")
            verifier.verify_release_metadata(RELEASE_PATH, release_path, True)

            release["assets"][0]["digest"] = "sha256:" + "0" * 64
            release_path.write_text(json.dumps(release), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "digest differs"):
                verifier.verify_release_metadata(RELEASE_PATH, release_path, True)

            release["assets"] = release["assets"][:1]
            release["assets"][0]["digest"] = "sha256:" + expected[
                release["assets"][0]["name"]
            ]
            release["draft"] = True
            release["prerelease"] = True
            release_path.write_text(json.dumps(release), encoding="utf-8")
            verifier.verify_release_metadata(
                RELEASE_PATH,
                release_path,
                False,
                allow_draft=True,
                allow_partial=True,
            )
            with self.assertRaisesRegex(ValueError, "is a draft"):
                verifier.verify_release_metadata(RELEASE_PATH, release_path, False)


if __name__ == "__main__":
    unittest.main()
