# ryeos:signed:2026-09-10T10:47:37Z:25e50a308d5611b0bf544e6c85458daa3c84d74c41d7d11e4e5a93144d39151d:qyD+QkzLCUGrm28Cw/SijltzskUHi2jn+6QUj5va+InzyIMVGzBrY+wutDxG2MqFKm8RRAx7j19Zk6ULwrVsBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
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

    def test_profile_references_the_complete_json_source_closure(self) -> None:
        profile = json.loads(PROFILE.read_text())
        referenced = {
            "structured-session.profile.json",
            profile["baseline_config"],
            profile["http_sse"]["readiness_schema"],
        }
        for route in profile["routes"]:
            referenced.update(
                (route["request_schema"], route["response_schema"], route["http_body_schema"])
            )
        referenced.update(rule["schema"] for rule in profile["notifications"])
        referenced.update(profile["ignored_notifications"].values())
        referenced.update(rule["schema"] for rule in profile["server_requests"])
        present = {
            path.relative_to(SOURCE).as_posix()
            for path in SOURCE.rglob("*.json")
            if path.is_file()
        }
        self.assertEqual(present, referenced)

    def test_worker_execution_never_grants_a_tool_wildcard(self) -> None:
        for path in EXECUTIONS:
            definition = yaml.safe_load(path.read_text())
            caps = definition["config"]["workload_client_delegation_caps"]
            self.assertNotIn("ryeos.execute.tool.*", caps)

    def test_no_raw_credential_enrollment_execution_is_published(self) -> None:
        self.assertFalse(
            (BUNDLE / ".ai/worker-executions/opencode/login.yaml").exists()
        )


if __name__ == "__main__":
    unittest.main()
