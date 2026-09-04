#!/usr/bin/env python3
"""Bundle-owned conformance tests for the pinned Codex integration data."""

from __future__ import annotations

import hashlib
import json
from decimal import Decimal, InvalidOperation
from pathlib import Path
import re
import stat
import unittest

import yaml


BUNDLE = Path(__file__).resolve().parent
SOURCE = BUNDLE / ".ai/workers/codex/lib/hosted"
PROFILE_PATH = SOURCE / "structured-session.profile.json"
WORKER_PATH = BUNDLE / ".ai/workers/codex/hosted.yaml"
ACTIVATION_PATH = BUNDLE / ".ai/config/codex/activation.yaml"
ENVIRONMENT_ACTIVATION_PATH = (
    BUNDLE / ".ai/config/codex/environment-activation.yaml"
)
ENVIRONMENT_PATH = BUNDLE / ".ai/config/codex/environments/default.yaml"
HOSTED_WORKFLOW_PROFILE_PATH = (
    BUNDLE.parent / ".ai/node/init/profiles/hosted-workflow.yaml"
)
README_PATH = BUNDLE / "README.md"
WORKER_EXECUTION_PATHS = (
    BUNDLE / ".ai/worker-executions/codex/login.yaml",
    BUNDLE / ".ai/worker-executions/codex/session.yaml",
)


def source_manifest_digest() -> str:
    entries = []
    total = 0
    for path in sorted(
        (candidate for candidate in SOURCE.rglob("*") if candidate.is_file()),
        key=lambda candidate: candidate.relative_to(SOURCE).as_posix().encode(),
    ):
        content = path.read_bytes()
        total += len(content)
        mode = path.stat().st_mode
        entries.append(
            {
                "root": "source",
                "path": path.relative_to(SOURCE).as_posix(),
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


class CodexContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.profile = json.loads(PROFILE_PATH.read_text(encoding="utf-8"))
        cls.routes = {route["id"]: route for route in cls.profile["routes"]}

    def test_managed_activation_closes_worker_files_and_environment_tree(self) -> None:
        activation = ACTIVATION_PATH.read_text(encoding="utf-8")
        environment_activation = ENVIRONMENT_ACTIVATION_PATH.read_text(
            encoding="utf-8"
        )
        environment = ENVIRONMENT_PATH.read_text(encoding="utf-8")

        self.assertIn("schema: ryeos.external_content_activation.v3", activation)
        self.assertIn("    maximum_entries: 64", activation)
        self.assertEqual(activation.count("      kind: mapped"), 5)
        self.assertEqual(activation.count("        target: null"), 5)
        for line in (
            "      - path: codex-resources/bwrap",
            "        sha256: 77360cb751ccedc5971391444ac86a8a33c15b04d6b4a6fe45f5d25496e62c4c",
            "  - id: codex-bwrap",
            "        member: codex-resources/bwrap",
        ):
            self.assertIn(line, activation)

        worker = WORKER_PATH.read_text(encoding="utf-8")
        bwrap_manifest = {
            "schema": "ryeos.external_content.large.v2",
            "kind": "external_large_content_manifest",
            "entries": [
                {
                    "path": "content",
                    "kind": "file",
                    "mode": 0o755,
                    "blob_hash": "77360cb751ccedc5971391444ac86a8a33c15b04d6b4a6fe45f5d25496e62c4c",
                    "size": 529776,
                }
            ],
            "entry_count": 1,
            "total_bytes": 529776,
        }
        bwrap_manifest_digest = hashlib.sha256(
            json.dumps(
                bwrap_manifest,
                ensure_ascii=False,
                separators=(",", ":"),
                sort_keys=True,
            ).encode()
        ).hexdigest()
        self.assertEqual(
            bwrap_manifest_digest,
            "5f2c25277b1a2150937372ade46eef46aa2227b749f984bea208b5467540b86f",
        )
        self.assertIn(f'    digest: "{bwrap_manifest_digest}"', worker)
        self.assertIn("    mount: codex-resources/bwrap", worker)
        self.assertIn(
            "schema: ryeos.external_content_activation.v3",
            environment_activation,
        )
        self.assertIn("      kind: mapped", environment_activation)
        self.assertIn(
            "consumer_ref: config:codex/environments/default",
            environment_activation,
        )
        for line in (
            "  - id: command-tools",
            "    storage: content",
            "        member: codex-path/rg",
            "        target: bin/rg",
            "        member: codex-resources/zsh/bin/zsh",
            "        target: bin/zsh",
        ):
            self.assertIn(line, environment_activation)
        expected_manifest = {
            "schema": "ryeos.external_content.tree.v2",
            "kind": "external_content_manifest",
            "entries": [
                {"path": "bin", "kind": "dir"},
                {
                    "path": "bin/rg",
                    "kind": "file",
                    "mode": 0o755,
                    "blob_hash": "e62198eb19b136b88c330af83647b5a962cb99b6b1f066758568f12de1974849",
                    "size": 5408904,
                },
                {
                    "path": "bin/zsh",
                    "kind": "file",
                    "mode": 0o755,
                    "blob_hash": "67faaaa89242c4a332e16e508a1977cffc24bf7fca31d4411cdfd101f3831ef3",
                    "size": 898480,
                },
            ],
            "entry_count": 3,
            "total_bytes": 6307384,
        }
        manifest_digest = hashlib.sha256(
            json.dumps(
                expected_manifest,
                ensure_ascii=False,
                separators=(",", ":"),
                sort_keys=True,
            ).encode()
        ).hexdigest()
        self.assertEqual(manifest_digest, "f1f39917086d223da68135108afa401fe75d47e2b102ea3f81c699595256bfe5")
        self.assertIn(f"    digest: {manifest_digest}", environment)
        self.assertIn("    - realization_id: command-tools", environment)
        self.assertIn("schema: ryeos.worker_environment.v3", environment)
        self.assertIn("  process_environment: {}", environment)
        self.assertIn("      relative_directory: bin", environment)

    def test_hosted_workflow_profile_admits_the_signed_worker(self) -> None:
        worker = yaml.safe_load(WORKER_PATH.read_text(encoding="utf-8"))
        profile = yaml.safe_load(
            HOSTED_WORKFLOW_PROFILE_PATH.read_text(encoding="utf-8")
        )
        sessions = profile["policies"]["persistent_sessions"]
        self.assertTrue(sessions["enabled"])
        self.assertGreaterEqual(
            sessions["limits"]["max_real_uid_process_limit"],
            worker["session_resources"]["real_uid_process_limit"],
        )

    def test_runbook_operator_grant_covers_workers_but_excludes_peer_services(self) -> None:
        readme = README_PATH.read_text(encoding="utf-8")
        match = re.search(r"(?m)^HOSTED_SCOPES='([^']+)'$", readme)
        self.assertIsNotNone(match, "runbook HOSTED_SCOPES declaration is absent")
        hosted_scopes = set(match.group(1).split(","))
        declared_runtime_scopes = set()
        for path in WORKER_EXECUTION_PATHS:
            body = "\n".join(path.read_text(encoding="utf-8").splitlines()[1:])
            execution = yaml.safe_load(body)
            declared_runtime_scopes.update(
                execution["requires"]["capabilities"]["declared"]
            )

        self.assertEqual(
            declared_runtime_scopes,
            {
                "ryeos.runtime.dedicated_session.start",
                "ryeos.runtime.dedicated_session.command",
                "ryeos.runtime.dedicated_session.terminate",
            },
        )
        self.assertLessEqual(declared_runtime_scopes, hosted_scopes)
        for internal_scope in (
            "ryeos.execute.service.objects/get",
            "ryeos.execute.service.objects/closure/get",
            "ryeos.execute.service.worker-placements/preflight",
            "ryeos.execute.service.worker-placements/prepare",
            "ryeos.execute.service.worker-placements/adopt",
            "ryeos.execute.service.worker-placements/abort",
            "ryeos.execute.service.federation/follow-terminal-deliver",
        ):
            self.assertNotIn(internal_scope, hosted_scopes)

    def test_portable_session_has_a_finite_conserved_execution_allowance(self) -> None:
        session_path = BUNDLE / ".ai/worker-executions/codex/session.yaml"
        body = "\n".join(session_path.read_text(encoding="utf-8").splitlines()[1:])
        session = yaml.safe_load(body)

        # worker-execution-runtime has no direct provider financial authority:
        # this is the finite RyeOS execution allowance that can be transferred
        # exactly across placements, not evidence of ChatGPT subscription spend.
        allowance = session["limits"]["spend_usd"]
        self.assertIsInstance(allowance, str)
        self.assertRegex(allowance, r"^(0|[1-9][0-9]*)(\.[0-9]+)?$")
        try:
            parsed = Decimal(allowance)
        except InvalidOperation as error:
            self.fail(f"session allowance is not a canonical decimal: {error}")
        self.assertGreater(parsed, Decimal(0))

    def test_every_mapped_codex_file_reconstructs_its_worker_manifest_pin(self) -> None:
        activation = ACTIVATION_PATH.read_text(encoding="utf-8")
        worker = WORKER_PATH.read_text(encoding="utf-8")
        files = {
            "codex": (
                "bin/codex",
                "cb0a15567e9a60a5820d54b0f6ae86d504dc3805c1eab21a47f70e3eb7b73a40",
                258_278_208,
                268_435_456,
                "713c3d1985ca438d8c309631d665c14f8fa0afedfce8b73dc93d0646edfe11ff",
                [
                    "89d1c6de1f2f2256926739b728f858de87be112b2be438c35c5e1cf574beaa77",
                    "b8ae5af9bd92025d7057ac8cc22b8f72aaf9e87e9f8be7c05c69771ee4411058",
                    "801ca86360d1a317acca723ada194d1078b7f9e8f5e501dc3e24ff69f76c1dec",
                    "b917dc5e71733ec589d3a665c840d64586172b84eebadcb6e50d1c9c56a49846",
                ],
            ),
            "codex-code-mode-host": (
                "bin/codex-code-mode-host",
                "00ecf5d040865b97884c488883abd342581c2a432debe7a54e4646bceee3d2d6",
                49_682_360,
                67_108_864,
                "984168b4c3f0efcbee4d1707fda8f8320bdb08a75cb704a8205f853e79e4dc2d",
                [
                    "00ecf5d040865b97884c488883abd342581c2a432debe7a54e4646bceee3d2d6"
                ],
            ),
            "codex-bwrap": (
                "codex-resources/bwrap",
                "77360cb751ccedc5971391444ac86a8a33c15b04d6b4a6fe45f5d25496e62c4c",
                529_776,
                1_048_576,
                "5f2c25277b1a2150937372ade46eef46aa2227b749f984bea208b5467540b86f",
                None,
            ),
            "codex-zsh": (
                "codex-resources/zsh/bin/zsh",
                "67faaaa89242c4a332e16e508a1977cffc24bf7fca31d4411cdfd101f3831ef3",
                898_480,
                2_097_152,
                "bda40827700404df0317f0e83951a4f6b6fb0933b3eec3af29c1a903c39aa008",
                None,
            ),
            "codex-rg": (
                "codex-path/rg",
                "e62198eb19b136b88c330af83647b5a962cb99b6b1f066758568f12de1974849",
                5_408_904,
                8_388_608,
                "a7a1a4fdb45e9231b80a9840c22728b05625ed1e21cc155d3995697eeeec22c0",
                None,
            ),
        }
        for component_id, (
            member,
            file_sha256,
            size,
            maximum,
            expected,
            chunk_hashes,
        ) in files.items():
            file_entry = {
                "path": "content",
                "kind": "file",
                "mode": 0o755,
                "size": size,
            }
            if chunk_hashes is None:
                file_entry["blob_hash"] = file_sha256
            else:
                file_entry["file_sha256"] = file_sha256
                file_entry["chunk_size"] = 64 * 1024 * 1024
                file_entry["chunk_hashes"] = chunk_hashes
            manifest = {
                "schema": "ryeos.external_content.large.v2",
                "kind": "external_large_content_manifest",
                "entries": [file_entry],
                "entry_count": 1,
                "total_bytes": size,
            }
            observed = hashlib.sha256(
                json.dumps(
                    manifest,
                    ensure_ascii=False,
                    separators=(",", ":"),
                    sort_keys=True,
                ).encode()
            ).hexdigest()
            self.assertEqual(observed, expected, component_id)
            member_block = (
                f"      - path: {member}\n"
                "        disposition: import\n"
                f"        sha256: {file_sha256}\n"
                f"        maximum_bytes: {maximum}\n"
                "        executable: true"
            )
            self.assertIn(member_block, activation, component_id)
            component_block = (
                f"  - id: {component_id}\n"
                "    storage: large_content\n"
                "    shape:\n"
                "      kind: mapped\n"
                "      members:\n"
                "        - source: codex-package\n"
                f"          member: {member}\n"
                "          target: null"
            )
            self.assertIn(component_block, activation, component_id)
            declaration = re.compile(
                rf"(?m)^  - id: {re.escape(component_id)}\n"
                rf"    kind: file\n"
                rf"    mode: pinned\n"
                rf"    digest: \"{expected}\"$"
            )
            self.assertRegex(worker, declaration, component_id)

    def test_authority_overrides_are_forbidden_even_when_null(self) -> None:
        for route_id in ("session.start", "session.resume", "turn.start"):
            route = self.routes[route_id]
            self.assertIn("approvalPolicy", route["forbidden_fields"])
        self.assertIn("ephemeral", self.routes["session.start"]["forbidden_fields"])

    def test_recovery_is_signed_data_and_stays_inside_its_route_sets(self) -> None:
        recovery = self.profile["recovery"]
        self.assertEqual(recovery["route_sets"], ["session"])
        session_routes = self.profile["route_sets"]["session"]
        for key in ("resume_route", "inspect_route"):
            route_id = recovery[key]
            self.assertIn(route_id, session_routes)
            route = self.routes[route_id]
            self.assertEqual(route["audience"], "runtime")
        self.assertEqual(
            self.routes[recovery["resume_route"]]["session_binding"]["action"],
            "bind_expected",
        )
        self.assertEqual(
            self.routes[recovery["inspect_route"]]["session_binding"]["action"],
            "require",
        )

        inspect = self.routes[recovery["inspect_route"]]
        outcomes = {
            observation["value"]["fields"]["outcome"]["value"]
            for observation in inspect["observations"]
        }
        self.assertEqual(outcomes, {"safe_idle", "uncertain"})
        safe_idle = next(
            observation
            for observation in inspect["observations"]
            if observation["value"]["fields"]["outcome"]["value"] == "safe_idle"
        )
        self.assertEqual(
            safe_idle["when"],
            [
                {
                    "pointer": "/response/result/thread/status/type",
                    "equals": "idle",
                }
            ],
        )

    def test_new_thread_is_persisted_before_ryeos_accepts_its_binding(self) -> None:
        start = self.routes["session.start"]
        self.assertEqual(start["post_success_routes"], ["session.persist"])

        persist = self.routes["session.persist"]
        self.assertIn("session.persist", self.profile["route_sets"]["session"])
        self.assertEqual(persist["method"], "thread/name/set")
        self.assertEqual(persist["audience"], "runtime")
        self.assertEqual(persist["effect_class"], "session_mutation")
        self.assertEqual(persist["session_binding"]["action"], "require")
        self.assertEqual(persist["session_binding"]["request_field"], "threadId")
        self.assertEqual(persist["observations"], [])
        self.assertEqual(persist["result_retention"], "ephemeral")

    def test_approval_protocol_is_entirely_profile_authored(self) -> None:
        self.assertTrue(self.profile["server_requests"])
        immutable_args = self.profile["workload_args"]
        self.assertIn(
            'approval_policy="on-request"',
            immutable_args,
        )
        baseline = (SOURCE / self.profile["baseline_config"]).read_text()
        self.assertIn('approval_policy = "on-request"', baseline)
        for route_id in ("session.start", "session.resume"):
            self.assertIn(
                {
                    "pointer": "/response/result/approvalPolicy",
                    "equals": "on-request",
                },
                self.routes[route_id]["response_predicates"],
            )
        for request in self.profile["server_requests"]:
            self.assertNotIn("response_style", request)
            self.assertEqual(
                set(request["responses"]), {"accept", "cancel", "decline", "expire"}
            )
            self.assertEqual(
                set(request["correlation"]),
                {"upstream_session_pointer", "operation_pointer"},
            )
            self.assertTrue(
                request["deny_only"],
                f"{request['method']} must not widen the immutable permission ceiling",
            )

    def test_portable_state_classifies_rebuildable_global_state(self) -> None:
        selectors = {
            selector["pattern"]: selector
            for selector in self.profile["portable_state"]["selectors"]
        }
        patterns = [
            selector["pattern"]
            for selector in self.profile["portable_state"]["selectors"]
        ]
        self.assertEqual(patterns, sorted(patterns))
        self.assertEqual(
            selectors["session_index.jsonl"],
            {
                "pattern": "session_index.jsonl",
                "class": "rebuildable_cache",
                "max_matches": 1,
            },
        )
        self.assertEqual(
            selectors["sessions/*/*/*/rollout-*-{session_id}.jsonl"]["class"],
            "portable_session_state",
        )
        self.assertEqual(selectors[".tmp/**"]["class"], "rebuildable_cache")
        self.assertEqual(selectors["tmp/**"]["class"], "rebuildable_cache")

    def test_worker_source_digest_covers_the_complete_profile_closure(self) -> None:
        worker = WORKER_PATH.read_text(encoding="utf-8")
        source = worker[worker.index("\nsource:\n") :]
        match = re.search(r'(?m)^  digest: "([0-9a-f]{64})"$', source)
        self.assertIsNotNone(match, "worker source digest is absent")
        self.assertEqual(match.group(1), source_manifest_digest())


if __name__ == "__main__":
    unittest.main()
