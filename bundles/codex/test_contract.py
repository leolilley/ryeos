#!/usr/bin/env python3
"""Bundle-owned conformance tests for the pinned Codex integration data."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re
import stat
import unittest


BUNDLE = Path(__file__).resolve().parent
SOURCE = BUNDLE / ".ai/workers/codex/lib/hosted"
PROFILE_PATH = SOURCE / "structured-session.profile.json"
WORKER_PATH = BUNDLE / ".ai/workers/codex/hosted.yaml"


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
