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
PROFILE_SPECS = {
    "qwen3-0.6b-cpu-4096": 4096,
    "qwen3-0.6b-cpu-2048": 2048,
}
ACTIVATION_PATHS = {
    profile: BUNDLE / f".ai/config/ryeos-runtime/{profile}-activation.yaml"
    for profile in PROFILE_SPECS
}
WORKER_PATHS = {
    profile: BUNDLE / f".ai/workers/local-inference/{profile}.yaml"
    for profile in PROFILE_SPECS
}
PROVIDER_PATHS = {
    profile: BUNDLE / f".ai/config/ryeos-runtime/model-providers/{profile}.yaml"
    for profile in PROFILE_SPECS
}
DIRECTIVE_PATHS = {
    profile: BUNDLE
    / f".ai/directives/local-inference/examples/{profile.replace('-', '_').replace('.', '_')}_smoke.md"
    for profile in PROFILE_SPECS
}
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
FULL_PROFILE_PATH = REPOSITORY / "bundles/.ai/node/init/profiles/full.yaml"
FULL_SANDBOX_PROFILE_PATH = (
    REPOSITORY / "bundles/.ai/node/init/profiles/full-sandbox.yaml"
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
        cls.activations = {
            profile: yaml.safe_load(path.read_text(encoding="utf-8"))
            for profile, path in ACTIVATION_PATHS.items()
        }
        cls.workers = {
            profile: yaml.safe_load(path.read_text(encoding="utf-8"))
            for profile, path in WORKER_PATHS.items()
        }
        cls.providers = {
            profile: yaml.safe_load(path.read_text(encoding="utf-8"))
            for profile, path in PROVIDER_PATHS.items()
        }
        cls.release = json.loads(RELEASE_PATH.read_text(encoding="utf-8"))

    def test_activation_is_a_closed_whole_tree_recipe(self) -> None:
        for profile, activation in self.activations.items():
            self.assertEqual(
                activation["schema"],
                "ryeos.external_content_activation.v3",
            )
            self.assertEqual(
                activation["consumer_ref"],
                f"worker:local-inference/{profile}",
            )
            self.assertNotIn("persistent_session_policy", activation)
            self.assertEqual(len(activation["sources"]), 4)
            self.assertEqual(len(activation["components"]), 4)
            self.assertTrue(
                all(not source.get("members") for source in activation["sources"])
            )
            self.assertTrue(
                all(
                    component["shape"]["kind"] == "whole_archive_tree"
                    for component in activation["components"]
                )
            )
            serialized = ACTIVATION_PATHS[profile].read_text(encoding="utf-8")
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
        for profile in PROFILE_SPECS:
            activation = self.activations[profile]
            worker = self.workers[profile]
            sources = {
                source["id"].removesuffix("_archive"): source
                for source in activation["sources"]
            }
            components = {
                component["id"]: component
                for component in activation["components"]
            }
            declarations = {
                declaration["id"]: declaration
                for declaration in worker["external_content"]
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
        self.assertTrue(policy["managed_activation"]["enabled"])
        managed = policy["managed_activation"]["limits"]
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

    def test_full_init_profiles_admit_the_exact_release(self) -> None:
        realizations = self.release["realizations"]
        bounds = [item["bounds"] for item in realizations]
        for path in (FULL_PROFILE_PATH, FULL_SANDBOX_PROFILE_PATH):
            profile = yaml.safe_load(path.read_text(encoding="utf-8"))
            external = profile["policies"]["external_content"]
            self.assertEqual(
                external["limits"]["max_depth"],
                max(item["maximum_depth"] for item in bounds),
                path,
            )
            self.assertEqual(
                external["limits"]["max_entries"],
                max(item["maximum_entries"] for item in bounds),
                path,
            )
            self.assertEqual(
                external["limits"]["max_file_bytes"],
                max(item["maximum_file_bytes"] for item in bounds),
                path,
            )
            self.assertEqual(
                external["limits"]["max_total_bytes"],
                max(item["maximum_total_bytes"] for item in bounds),
                path,
            )
            self.assertEqual(
                external["limits"]["store_budget_bytes"], 4 * 1024**3, path
            )
            self.assertTrue(external["managed_activation"]["enabled"], path)
            managed = external["managed_activation"]["limits"]
            self.assertEqual(
                set(managed["allowed_https_hosts"]),
                {
                    "github.com",
                    "release-assets.githubusercontent.com",
                    "releases.openai.com",
                },
                path,
            )
            self.assertEqual(managed["max_archives"], len(realizations), path)
            self.assertEqual(
                managed["max_compressed_bytes"],
                sum(item["maximum_compressed_bytes"] for item in realizations),
                path,
            )
            self.assertEqual(
                managed["max_expanded_bytes"],
                sum(item["maximum_expanded_bytes"] for item in realizations),
                path,
            )
            self.assertEqual(
                managed["max_members"],
                sum(item["maximum_entries"] for item in realizations),
                path,
            )
            self.assertEqual(
                managed["max_member_bytes"],
                max(item["maximum_file_bytes"] for item in bounds),
                path,
            )
            sessions = profile["policies"]["persistent_sessions"]
            self.assertTrue(sessions["enabled"], path)
            self.assertEqual(sessions["limits"]["max_real_uid_process_limit"], 4096, path)

    def test_worker_source_digest_covers_the_complete_source_closure(self) -> None:
        for path in WORKER_PATHS.values():
            source = path.read_text(encoding="utf-8")
            match = re.search(r'(?m)^  digest: "([0-9a-f]{64})"$', source)
            self.assertIsNotNone(match, "worker source digest is absent")
            self.assertEqual(match.group(1), worker_source_manifest_digest())

    def test_worker_declares_its_shared_real_uid_process_ceiling(self) -> None:
        for profile, process_limit in PROFILE_SPECS.items():
            self.assertEqual(
                self.workers[profile]["session_resources"],
                {"real_uid_process_limit": process_limit},
            )

    def test_profile_selection_is_signed_and_has_no_predecessor_alias(self) -> None:
        for profile, provider in self.providers.items():
            self.assertEqual(
                provider["transport"]["execute"],
                f"worker:local-inference/{profile}",
            )
            directive = DIRECTIVE_PATHS[profile].read_text(encoding="utf-8")
            self.assertIn(f"provider: {profile}", directive)
        for predecessor in (
            BUNDLE / ".ai/workers/local-inference/local-tinygrad.yaml",
            BUNDLE / ".ai/config/ryeos-runtime/local-tinygrad-activation.yaml",
            BUNDLE / ".ai/config/ryeos-runtime/model-providers/local-tinygrad.yaml",
            BUNDLE / ".ai/directives/local-inference/examples/tinygrad_smoke.md",
        ):
            self.assertFalse(predecessor.exists(), predecessor)

    def test_two_profiles_share_realizations_but_move_signed_worker_identity(self) -> None:
        names = list(PROFILE_SPECS)
        first_worker = self.workers[names[0]]
        second_worker = self.workers[names[1]]
        self.assertEqual(first_worker["source"], second_worker["source"])
        self.assertEqual(
            first_worker["external_content"], second_worker["external_content"]
        )
        self.assertEqual(first_worker["config"], second_worker["config"])
        self.assertNotEqual(
            first_worker["session_resources"], second_worker["session_resources"]
        )
        first_activation = dict(self.activations[names[0]])
        second_activation = dict(self.activations[names[1]])
        self.assertNotEqual(
            first_activation.pop("consumer_ref"),
            second_activation.pop("consumer_ref"),
        )
        self.assertEqual(first_activation, second_activation)

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
        self.assertEqual(source_qualifier.count("--evidence-output"), 2)
        self.assertIn(
            "actions/upload-artifact@b7c566a772e6b6bfb58ed0dc250532a479d7789f",
            source_qualifier,
        )
        self.assertIn("if-no-files-found: error", source_qualifier)
        self.assertIn("local-inference-offline-qualification.json", source_qualifier)
        self.assertIn("local-inference-online-qualification.json", source_qualifier)
        self.assertIn("--archive-root", qualifier)
        self.assertIn("minimum_free_bytes = int(sys.argv[4])", qualifier)
        self.assertIn('source "$repository_root/scripts/pkg/bundle-sets.sh"', qualifier)
        self.assertIn("ryeos_bundle_set_names full", qualifier)
        self.assertIn('--source "$qualification_source"', qualifier)
        self.assertIn("--node-profile full", qualifier)
        self.assertIn('minimum_free_bytes="2147483648"', qualifier)
        self.assertIn(
            '"$node_root/.ai/node/policies/external_content.yaml"', qualifier
        )
        self.assertIn('"external_content_policy": json.loads(', qualifier)
        self.assertNotIn('--source "$bundle_source"', qualifier)
        self.assertNotIn("shutil.copyfile", qualifier)
        self.assertIn(
            'python3 - "$qualification_root" <<\'PY\'',
            qualifier,
        )
        self.assertIn('max_total_address_space_bytes"] = 1', qualifier)
        self.assertIn("provider_attempt_reservation", qualifier)
        self.assertIn("namespace='provider.call'", qualifier)
        self.assertIn("provider_call_observation_recorded", qualifier)
        for profile in PROFILE_SPECS:
            self.assertIn(
                f"config:ryeos-runtime/{profile}-activation", qualifier
            )
            self.assertIn(
                f"directive:local-inference/examples/{profile.replace('-', '_').replace('.', '_')}_smoke",
                qualifier,
            )
            self.assertIn(f"validation-before-$profile.json", qualifier)
            self.assertIn(f"validation-after-$profile.json", qualifier)
            self.assertIn(f"validation-refused-$profile.json", qualifier)
            self.assertIn(f"validation-released-$profile.json", qualifier)
        self.assertIn("runtime_preparation", qualifier)
        self.assertIn("static validation changed thread inventory", qualifier)
        self.assertIn("ready static validation changed thread inventory", qualifier)
        self.assertIn("refusal validation changed thread inventory", qualifier)
        self.assertIn("released-binding validation changed thread inventory", qualifier)
        self.assertIn("released exact binding remained ready", qualifier)
        self.assertIn("consumer-specific release affected the other exact profile", qualifier)
        self.assertIn("execution reused validation after its exact binding was released", qualifier)
        self.assertIn('"validation": {', qualifier)
        self.assertIn('"validation_thread_inventory": {', qualifier)
        self.assertIn("dependency resolution identity moved across phases", qualifier)
        self.assertIn("provider_call_objects", qualifier)
        for field in (
            "effective_definition_digest",
            "capsule_hash",
            "execution_realization_hash",
            "provider_config_hash",
            "provider_config_value_digest",
        ):
            self.assertIn(field, qualifier)
        self.assertIn(
            'snapshot_provider_bank "$qualification_root/bank-before-replay.json" exact-profiles',
            qualifier,
        )
        refusal_position = qualifier.index(
            '"$policy_root/persistent-sessions-refusal.json" --app-root'
        )
        restore_position = qualifier.index(
            '"$policy_root/persistent-sessions.json" --app-root',
            refusal_position,
        )
        execution_position = qualifier.index(
            '"$qualification_root/executed-4096.json"'
        )
        bank_position = qualifier.index(
            'snapshot_provider_bank "$qualification_root/bank-before-replay.json" exact-profiles'
        )
        replay_position = qualifier.index(
            '"$qualification_root/replayed-4096.json"'
        )
        self.assertLess(refusal_position, restore_position)
        self.assertLess(restore_position, execution_position)
        self.assertLess(execution_position, bank_position)
        self.assertLess(bank_position, replay_position)
        self.assertNotIn("replay-zero-capacity", qualifier)
        self.assertIn(
            'snapshot_provider_bank "$qualification_root/bank-before-release-refusal.json" state-only',
            qualifier,
        )
        self.assertIn(
            'snapshot_provider_bank "$qualification_root/bank-after-release-refusal.json" state-only',
            qualifier,
        )
        self.assertIn('validation_mode == "state-only"', qualifier)
        self.assertIn('"persistent_session_policy_transition": json.loads(', qualifier)
        for policy_proof in (
            'snapshot_persistent_session_policy',
            'persistent-policy-baseline.json',
            'persistent-policy-refusal.json',
            'persistent-policy-restored.json',
            'persistent-policy-transition.json',
            '"schema": "ryeos.local_inference_persistent_session_policy_snapshot.v1"',
            '"schema": "ryeos.local_inference_persistent_session_policy_transition.v1"',
            'restored["body"] != baseline["body"]',
            'restored["signed_content_hash"] != baseline["signed_content_hash"]',
            'refusal["body"] != expected_refusal',
        ):
            self.assertIn(policy_proof, qualifier)
        self.assertNotIn('persistent = {', qualifier)
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
        self.assertEqual(
            qualifier.count("expected = {}"),
            3,
            "zero-argument tools must receive only their declared empty input",
        )
        for implicit_runtime_input in (
            '"cancellation_grace_secs": 5',
            '"cancellation_mode": "graceful"',
            '"project_path": project_path',
            '"timeout": 86400',
        ):
            self.assertNotIn(implicit_runtime_input, qualifier)
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

    def test_release_qualification_pins_exact_profile_and_release_authority(self) -> None:
        qualifier = NODE_QUALIFIER_PATH.read_text(encoding="utf-8")

        # Provider evidence is a relational proof, not two unordered sets of
        # distinct values. Each call record joins its provider/worker pair to
        # the exact accounting attempt and the corresponding execution thread.
        self.assertIn('expected_profiles = {', qualifier)
        for profile in PROFILE_SPECS:
            self.assertIn(
                f'"{profile}": "worker:local-inference/{profile}"',
                qualifier,
            )
        for required_join in (
            'attempts_by_id = {item["attempt_id"]: item for item in attempts}',
            'provider_id = record["coordinate"]["provider_id"]',
            'transport.get("worker_ref") != expected_worker',
            'attempts_by_id.get(observation.get("attempt_id"))',
            '"config_hash": record["coordinate"]["provider_config_hash"]',
            '"authority_digest": record["coordinate"]["authority_digest"]',
            '"thread_id": expected_thread',
            'observation.get("produced_by_thread") != expected_thread',
            'execution-thread-{profile.rsplit(\'-\', 1)[-1]}.json',
            'write_new_thread_proof_for_item',
            'threads-before-execution-4096.json',
            'threads-after-execution-4096.json',
            'threads-before-execution-2048.json',
            'threads-after-execution-2048.json',
            'if len(new) != 1 or len(matches) != 1:',
            '"schema": "ryeos.local_inference_execution_thread_proof.v1"',
            '"before_thread_ids": before',
            '"after_thread_ids": sorted(',
            '"new_threads": [',
            '"selected_thread_id": thread_id',
            '"execution_threads": {',
            'threads-before-replay-4096.json',
            'threads-after-replay-4096.json',
            'threads-before-replay-2048.json',
            'threads-after-replay-2048.json',
            'replay-thread-{profile.rsplit(\'-\', 1)[-1]}.json',
            'expected_replay_threads = {',
            'provider_calls_by_cache_key = {',
            'provider_calls_by_cache_key[record["cache_key"]]["coordinate"]',
            'replayed["thread_id"] != expected_replay_thread',
            '"replay_threads": {',
        ):
            self.assertIn(required_join, qualifier)

        # Cleanup is part of the release result: a successful body cannot
        # suppress a failed exact-node stop and then delete the live root.
        self.assertIn('local stop_failed=0', qualifier)
        self.assertIn('status=1', qualifier)
        self.assertIn('cleanup could not prove node stop', qualifier)
        self.assertIn('trap - EXIT', qualifier)
        self.assertIn('exit "$status"', qualifier)
        self.assertNotIn('"$ryeos_bin" stop --app-root "$node_root" >/dev/null 2>&1 || true', qualifier)

        # The cache/lease negative must occur in one daemon generation and
        # without a validation between release and launch.
        ready = qualifier.index(
            'validation-release-ready-qwen3-0.6b-cpu-4096.json'
        )
        release = qualifier.index(
            'thread_service service:external-content/release', ready
        )
        launch = qualifier.index('released_refusal_raw=', release)
        released_projection = qualifier.index(
            'validation-released-$profile.json', launch
        )
        self.assertLess(ready, release)
        self.assertLess(release, launch)
        self.assertLess(launch, released_projection)
        self.assertNotIn("stop_node", qualifier[ready:released_projection])
        self.assertNotIn(" validate ", qualifier[release:launch])

        # Only the exact structured absence is accepted. Raw rendered CLI
        # diagnostics are transient and the retained evidence is closed JSON.
        for exact_contract in (
            'status != 404',
            'body.get("code") != "external_content_binding_unavailable"',
            'body.get("retryable") is not False',
            'expected_binding = "worker:local-inference/qwen3-0.6b-cpu-4096"',
            'threads-before-release-refusal.json',
            'threads-after-release-refusal.json',
            'bank-before-release-refusal.json',
            'bank-after-release-refusal.json',
            'if before != after:',
            '"worker_contact": False',
            '"schema": "ryeos.local_inference_binding_refusal_proof.v1"',
            'released-binding-launch-refusal.json',
        ):
            self.assertIn(exact_contract, qualifier)
        self.assertIn(
            'rm -f -- "$qualification_root/.released-binding-launch-refusal.raw"',
            qualifier,
        )
        self.assertNotIn("released-binding-launch-refusal.txt", qualifier)

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
