# ryeos:signed:2026-09-10T10:35:03Z:f441e4148f2c0156feab253e543d7c0f7af9e680f6e5f2ad2f74a614e46f677d:AyAnAYaPTsfA9mv9QA9G5bbexbNH0rodh7nc3bhwSLAraRTyBfTwZSbfbRn7Vup70FshLp4gm4F+3py1RNt4DQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/env python3
"""Bundle-owned conformance checks for the OpenCode provider data."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re
import stat
import unittest

import yaml


BUNDLE = Path(__file__).resolve().parent
SOURCE = BUNDLE / ".ai/workers/opencode/lib/hosted"
PROFILE = SOURCE / "structured-session.profile.json"
WORKER = BUNDLE / ".ai/workers/opencode/hosted.yaml"
EXECUTIONS = (
    BUNDLE / ".ai/worker-executions/opencode/login.yaml",
    BUNDLE / ".ai/worker-executions/opencode/session.yaml",
    BUNDLE / ".ai/worker-executions/opencode/bounded-turn.yaml",
)


def source_digest() -> str:
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
    return hashlib.sha256(
        json.dumps(manifest, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode()
    ).hexdigest()


class OpenCodeContractTests(unittest.TestCase):
    def test_profile_uses_the_closed_http_projection_contract(self) -> None:
        profile = json.loads(PROFILE.read_text())
        self.assertEqual(profile["schema_version"], 6)
        self.assertEqual(profile["transport"], "http_sse")
        self.assertEqual(profile["http_sse"]["event_path"], "/event")
        self.assertEqual(profile["http_sse"]["readiness_path"], "/global/health")
        self.assertNotIn("credential.auth.set", profile["route_sets"]["enrollment"])
        self.assertNotIn("credential.auth.set", [route["id"] for route in profile["routes"]])
        for route in profile["routes"]:
            self.assertIn("http_body_schema", route)
            self.assertIn("http_path_parameters", route)
        turn = next(route for route in profile["routes"] if route["id"] == "turn.start")
        self.assertEqual(turn["fixed_params"], {"agent": "build"})
        for field in ("command", "model", "system", "tools", "variant"):
            self.assertIn(field, turn["forbidden_fields"])

    def test_source_closure_digest_is_exact(self) -> None:
        worker = WORKER.read_text(encoding="utf-8")
        source = worker[worker.index("\nsource:\n") :]
        match = re.search(r'(?m)^  digest: "([0-9a-f]{64})"$', source)
        self.assertIsNotNone(match)
        self.assertEqual(match.group(1), source_digest())

    def test_worker_execution_never_grants_a_tool_wildcard(self) -> None:
        for path in EXECUTIONS:
            definition = yaml.safe_load(path.read_text())
            caps = definition["config"]["workload_client_delegation_caps"]
            self.assertNotIn("ryeos.execute.tool.*", caps)


if __name__ == "__main__":
    unittest.main()
