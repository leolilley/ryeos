#!/usr/bin/env python3
# ryeos:signed:2026-11-16T12:48:09Z:abf49f26309154440805ce14abab31d541cd994db1ad02d20c68900c3a6ee0f7:1xzHbvBgLx3QJAvU+pkVllA3aNPhtZYdq6p5EjEfjs501SLDBAoxlvUswlqpu6/RT48bPt82woLEXXEeOgMVDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Bundle-owned conformance checks for the claude-code provider data.

These checks pin the authored shape the Rust validators judge: the profile
compiles in `structured_session_profile::compile` (see the
`claude_code_stdio_profile_compiles_from_the_exact_local_source_set` test) and
each environment passes the launch preparer's `validate_worker_environment`
(see `claude_code_environments_pass_the_launch_preparer_validator`). This
file proves the cross-file facts those tests cannot: digests, closures and the
worker, execution and environment agreement.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re
import stat
import unittest

import yaml


BUNDLE = Path(__file__).resolve().parent
SOURCE = BUNDLE / ".ai/workers/claude-code/lib/hosted"
PROFILE = SOURCE / "structured-session.profile.json"
WORKERS = {
    "hosted": BUNDLE / ".ai/workers/claude-code/hosted.yaml",
    "trusted-hosted": BUNDLE / ".ai/workers/claude-code/trusted-hosted.yaml",
}
# Each environment: its worker and the secrets it delivers. A model key
# (optionally through a gateway) or a Claude subscription token.
KEY_NAMES = ["ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"]
SUBSCRIPTION_NAMES = ["CLAUDE_CODE_OAUTH_TOKEN"]
ENVIRONMENTS = {
    "default": ("hosted", KEY_NAMES),
    "trusted-lane": ("trusted-hosted", KEY_NAMES),
    "subscription": ("hosted", SUBSCRIPTION_NAMES),
    "subscription-trusted-lane": ("trusted-hosted", SUBSCRIPTION_NAMES),
}
EXECUTIONS = {
    "bounded-turn": BUNDLE / ".ai/worker-executions/claude-code/bounded-turn.yaml",
    "bounded-turn-read-only": BUNDLE / ".ai/worker-executions/claude-code/bounded-turn-read-only.yaml",
    "bounded-turn-recovery": BUNDLE / ".ai/worker-executions/claude-code/bounded-turn-recovery.yaml",
    "bounded-turn-continue": BUNDLE / ".ai/worker-executions/claude-code/bounded-turn-continue.yaml",
}
COMMANDS = {
    "create": (BUNDLE / ".ai/node/commands/claude-code-profile-create.yaml", KEY_NAMES),
    "create-subscription": (
        BUNDLE / ".ai/node/commands/claude-code-subscription-profile-create.yaml",
        SUBSCRIPTION_NAMES,
    ),
}

TOP_LEVEL_KEYS = {
    "schema_version", "external_candidate", "transport", "http_sse",
    "configuration_authority", "workload_realization_id", "workload_executable",
    "workload_args", "workload_home_env", "required_process_environment",
    "workload_client", "baseline_config", "baseline_destination",
    "auxiliary_configs", "runtime_configs", "configuration_absence",
    "portable_state", "credential_subject", "initialization", "recovery",
    "route_sets", "routes", "notifications", "ignored_notifications",
    "server_requests",
}


def load_yaml(path: Path):
    return yaml.safe_load(path.read_text(encoding="utf-8"))


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


class ClaudeCodeContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.profile = json.loads(PROFILE.read_text())

    def test_profile_declares_every_required_top_level_key_and_nothing_else(self) -> None:
        self.assertEqual(set(self.profile), TOP_LEVEL_KEYS)
        self.assertEqual(self.profile["schema_version"], 13)
        self.assertEqual(self.profile["transport"], "stdio_jsonrpc")
        self.assertIsNone(self.profile["http_sse"])
        self.assertIsNone(self.profile["external_candidate"])
        self.assertEqual(self.profile["configuration_authority"], "immutable_argv")

    def test_profile_has_no_credential_subject_portable_state_or_delegation(self) -> None:
        self.assertIsNone(self.profile["credential_subject"])
        self.assertIsNone(self.profile["portable_state"])
        self.assertIsNone(self.profile["workload_client"])
        self.assertEqual(self.profile["server_requests"], [])
        self.assertEqual(self.profile["auxiliary_configs"], [])
        self.assertEqual(self.profile["runtime_configs"], [])

    def test_workload_is_the_captured_adapter_and_baseline_lands_under_the_home(self) -> None:
        self.assertEqual(self.profile["workload_realization_id"], "adapter")
        self.assertEqual(self.profile["workload_executable"], "claude-code-adapter")
        baseline = SOURCE / self.profile["baseline_config"]
        self.assertTrue(json.loads(baseline.read_text()))
        # One bare file name: the daemon writes it directly under the home.
        self.assertNotIn("/", self.profile["baseline_destination"])
        absence = self.profile["configuration_absence"]
        self.assertTrue(absence["absolute_paths"])
        for key in ("absolute_paths", "startup_ancestor_paths", "route_ancestor_paths"):
            self.assertEqual(absence[key], sorted(set(absence[key])), key)
        # The profile fixes no credential: each environment delivers one auth
        # path, and the adapter refuses a session without exactly one.
        self.assertEqual(self.profile["required_process_environment"], [])

    def test_initialization_is_pure_read(self) -> None:
        self.assertTrue(self.profile["initialization"])
        for step in self.profile["initialization"]:
            self.assertEqual(step["effect_class"], "pure_read")

    def test_routes_sets_and_recovery_agree(self) -> None:
        routes = {route["id"]: route for route in self.profile["routes"]}
        self.assertEqual(
            set(routes),
            {
                "session.start", "session.start_read_only", "session.resume",
                "session.read", "turn.start", "turn.interrupt",
            },
        )
        methods = [route["method"] for route in self.profile["routes"]]
        self.assertEqual(len(methods), len(set(methods)))
        for name, members in self.profile["route_sets"].items():
            self.assertEqual(members, sorted(set(members)), name)
            self.assertTrue(set(members) <= set(routes), name)
        recovery = self.profile["recovery"]
        self.assertEqual(recovery["resume_route"], "session.resume")
        self.assertEqual(recovery["inspect_route"], "session.read")
        self.assertEqual(recovery["route_sets"], sorted(recovery["route_sets"]))
        for name in recovery["route_sets"]:
            self.assertIn("session.resume", self.profile["route_sets"][name])
            self.assertIn("session.read", self.profile["route_sets"][name])
        for rid in ("session.resume", "session.read"):
            self.assertEqual(routes[rid]["audience"], "runtime")
        self.assertEqual(routes["session.resume"]["session_binding"]["action"], "bind_expected")
        self.assertEqual(routes["session.read"]["session_binding"]["action"], "require")
        self.assertEqual(routes["session.start"]["fixed_params"], {"access": "read_write"})
        self.assertEqual(routes["session.start_read_only"]["fixed_params"], {"access": "read_only"})
        self.assertNotIn("session.start", self.profile["route_sets"]["read_only"])
        # A continuation resumes a predecessor's conversation: it carries the
        # recovery pair and the turn routes, and no route that starts one.
        self.assertEqual(
            self.profile["route_sets"]["continue"],
            ["session.read", "session.resume", "turn.interrupt", "turn.start"],
        )
        self.assertEqual(recovery["route_sets"], ["continue", "read_only", "session"])
        self.assertNotIn("session.start_read_only", self.profile["route_sets"]["session"])
        for route in self.profile["routes"]:
            self.assertNotIn(route["effect_class"], ("credential_read", "credential_write", "credential_delete"))

    def test_turn_state_is_observed_from_start_and_completion(self) -> None:
        notifications = {rule["method"]: rule for rule in self.profile["notifications"]}
        self.assertEqual(set(notifications), {"turn/started", "turn/completed"})
        started = notifications["turn/started"]["observations"][0]["value"]["fields"]
        self.assertEqual(started["expected"]["value"], "idle")
        self.assertEqual(started["next"]["value"], "turn_running")
        self.assertIn("turn_id", started)
        done = notifications["turn/completed"]
        self.assertTrue(done["durable"])
        finished = done["observations"][0]["value"]["fields"]
        self.assertEqual(finished["expected"]["value"], "turn_running")
        self.assertEqual(finished["next"]["value"], "idle")
        self.assertIn("completed_turn_id", finished)
        self.assertIn("result", done["payload"]["fields"])
        self.assertIn("status", done["payload"]["fields"])
        self.assertTrue(done["result"])
        self.assertFalse(notifications["turn/started"]["result"])
        turn_start = next(route for route in self.profile["routes"] if route["id"] == "turn.start")
        self.assertEqual(turn_start["progress_notifications"], ["turn/started"])
        self.assertEqual(
            set(self.profile["ignored_notifications"]), {"turn/progress"},
            "progress is validated and dropped, never retained",
        )

    def test_profile_references_exactly_the_json_source_closure(self) -> None:
        referenced = {"structured-session.profile.json", self.profile["baseline_config"]}
        for step in self.profile["initialization"]:
            if step["response_schema"]:
                referenced.add(step["response_schema"])
        for route in self.profile["routes"]:
            referenced.update((route["request_schema"], route["response_schema"]))
        referenced.update(rule["schema"] for rule in self.profile["notifications"])
        referenced.update(self.profile["ignored_notifications"].values())
        present = {
            path.relative_to(SOURCE).as_posix()
            for path in SOURCE.rglob("*.json")
            if path.is_file()
        }
        self.assertEqual(present, referenced)
        for path in SOURCE.rglob("schema/*.json"):
            schema = json.loads(path.read_text())
            self.assertEqual(schema.get("type"), "object", path.name)

    def test_workers_pin_the_source_closure_and_capture_the_bundle_binary(self) -> None:
        protocols = {
            "hosted": "protocol:ryeos/core/structured_session",
            "trusted-hosted": "protocol:ryeos/core/trusted_structured_session",
        }
        for name, path in WORKERS.items():
            worker = load_yaml(path)
            self.assertEqual(worker["executor_id"], "@subprocess", name)
            self.assertEqual(worker["execution_protocol"], protocols[name], name)
            text = path.read_text(encoding="utf-8")
            match = re.search(r'(?m)^  digest: "([0-9a-f]{64})"$', text[text.index("\nsource:\n"):])
            self.assertIsNotNone(match, name)
            self.assertEqual(match.group(1), source_digest(), name)
            self.assertEqual(
                worker["external_content"],
                [{
                    "id": "adapter",
                    "kind": "file",
                    "bundle_binary": "bin:claude-code-adapter",
                    "mode": "captured",
                    "mount_root": "execution_runtime",
                    "mount": "claude-code-adapter",
                }],
                name,
            )
            self.assertEqual(
                worker["external_content"][0]["mount"], self.profile["workload_executable"]
            )
            self.assertEqual(
                worker["external_content"][0]["id"], self.profile["workload_realization_id"]
            )

    def test_environments_deliver_one_auth_path_and_name_their_worker(self) -> None:
        for name, (worker, names) in ENVIRONMENTS.items():
            environment = load_yaml(BUNDLE / f".ai/config/claude-code/environments/{name}.yaml")
            self.assertEqual(environment["schema"], "ryeos.worker_environment.v7", name)
            self.assertEqual(environment["worker_ref"], f"worker:claude-code/{worker}", name)
            self.assertEqual(
                environment["credential_requirement"],
                {
                    "workload_family": "claude-code",
                    "required_state": "active",
                    "subject_projection_contract": "ryeos.credential.delivered_secret.v1",
                },
                name,
            )
            self.assertEqual(environment["portable_state_contract"], "ryeos.worker_session.restore.v1")
            self.assertIsNone(environment["workload_client"])
            variables = environment["configuration"]["process_environment"]
            delivered = {
                variable: value
                for variable, value in variables.items()
                if value.get("kind") == "delivered_secret"
            }
            self.assertEqual(set(delivered), set(names), name)
            for variable, value in delivered.items():
                self.assertEqual(value, {"kind": "delivered_secret", "name": variable}, name)
            # Everything else is a plain literal: no other kind may carry a value.
            for variable, value in variables.items():
                if variable not in delivered:
                    self.assertEqual(value.get("kind"), "literal", f"{name}: {variable}")

    def test_executions_select_their_route_sets_without_durable_requirements(self) -> None:
        for name, path in EXECUTIONS.items():
            execution = load_yaml(path)
            config = execution["config"]
            self.assertEqual(config["required_credential_state"], "active", name)
            self.assertEqual(config["workload_client_delegation_caps"], [], name)
            self.assertEqual(config["mode"]["kind"], "bounded_turn", name)
            self.assertEqual(config["mode"]["turn_start_route"], "turn.start", name)
            self.assertIn(config["route_set"], self.profile["route_sets"], name)
            if name == "bounded-turn-continue":
                # The closed continuation shape: no session-start route at all.
                self.assertEqual(
                    set(config["mode"]),
                    {"kind", "continuation", "turn_start_route", "max_uncontacted_attempts"},
                )
                self.assertEqual(config["mode"]["continuation"], "completed_predecessor")
                self.assertNotIn(
                    "session.start", self.profile["route_sets"][config["route_set"]]
                )
                self.assertNotIn(
                    "session.start_read_only", self.profile["route_sets"][config["route_set"]]
                )
            else:
                self.assertIn(config["mode"]["session_start_route"], self.profile["route_sets"][config["route_set"]], name)
            self.assertIn(config["mode"]["turn_start_route"], self.profile["route_sets"][config["route_set"]], name)
            self.assertTrue(
                set(config["allowed_effect_classes"])
                >= {route["effect_class"] for route in self.profile["routes"]},
                name,
            )
            self.assertNotIn("node", execution["requires"], name)
            declared = " ".join(execution["requires"]["capabilities"]["declared"])
            self.assertNotIn("durable_state", declared, name)
            self.assertNotIn("private_home", declared, name)
            self.assertNotIn("persistence:durable_state", path.read_text(), name)
            self.assertNotIn("credential:private_home", path.read_text(), name)
        self.assertEqual(load_yaml(EXECUTIONS["bounded-turn"])["config"]["route_set"], "session")
        self.assertEqual(load_yaml(EXECUTIONS["bounded-turn-read-only"])["config"]["route_set"], "read_only")
        self.assertEqual(load_yaml(EXECUTIONS["bounded-turn-continue"])["config"]["route_set"], "continue")
        self.assertTrue(
            load_yaml(EXECUTIONS["bounded-turn-recovery"])["config"]["mode"]["require_post_completion_recovery"]
        )

    def test_profile_commands_activate_from_their_environments_names(self) -> None:
        for verb, (path, names) in COMMANDS.items():
            command = load_yaml(path)
            self.assertEqual(command["tokens"], ["claude-code", "profile", verb])
            self.assertEqual(
                command["defaults"]["activation"],
                {"kind": "delivered_secret", "names": sorted(names)},
                verb,
            )
            self.assertEqual(
                command["dispatch"]["execute"], "service:credential-profiles/create", verb
            )

    def test_adapter_is_not_a_scripted_workload_and_ships_in_no_profile_source(self) -> None:
        for path in SOURCE.rglob("*"):
            if path.is_file():
                self.assertFalse(path.stat().st_mode & stat.S_IXUSR, path.name)


if __name__ == "__main__":
    unittest.main()
