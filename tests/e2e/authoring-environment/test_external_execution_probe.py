"""Fixture boundary tests; no vendor binary, network, daemon, model or Rust build."""

import importlib.util
from pathlib import Path
import sys
import time
import tomllib
import unittest
from unittest.mock import patch
from unittest.mock import Mock


SPEC = importlib.util.spec_from_file_location(
    "external_execution_probe", Path(__file__).with_name("probe_pinned_codex_external_execution.py"))
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)


class ExternalExecutionProbeTests(unittest.TestCase):
    def test_configuration_uses_only_credential_free_scripted_provider(self):
        config = tomllib.loads(probe.fixture_config("http://127.0.0.1:12345"))
        provider = config["model_providers"][config["model_provider"]]
        self.assertFalse(provider["requires_openai_auth"])
        self.assertFalse(provider["supports_websockets"])
        self.assertEqual(provider["request_max_retries"], 0)
        self.assertEqual(provider["stream_max_retries"], 0)
        self.assertEqual(provider["base_url"], "http://127.0.0.1:12345")
        self.assertEqual(config["mcp_servers"], {})
        self.assertFalse(config["features"]["hooks"])
        self.assertFalse(config["features"]["plugins"])
        self.assertFalse(config["features"]["skill_mcp_dependency_install"])
        self.assertEqual(config["notify"], [])
        self.assertFalse(config["orchestrator"]["skills"]["enabled"])
        self.assertFalse(config["orchestrator"]["mcp"]["enabled"])
        self.assertFalse(config["features"]["multi_agent"])
        self.assertFalse(config["features"]["code_mode"]["enabled"])

    def test_isolated_launch_exposes_selected_artifacts_not_package_tree(self):
        argv = probe.isolated_command(Path("/p/bwrap"), Path("/package"),
                                      Path("/candidate"), Path("/profile"), ["exec-server"])
        self.assertIn("--clearenv", argv)
        self.assertIn("--unshare-pid", argv)
        self.assertNotIn("/package", argv)
        self.assertNotIn("/home", argv)
        self.assertIn("/package/bin/codex", argv)
        self.assertEqual(argv[-2:], ["/runtime/bin/codex", "exec-server"])

    def test_managed_requirements_fixture_uses_exact_system_path_read_only(self):
        argv = probe.isolated_command(Path("/p/bwrap"), Path("/package"),
            Path("/candidate"), Path("/profile"), ["app-server"],
            managed_requirements=Path("/fixture/requirements.toml"))
        offset = argv.index("/fixture/requirements.toml")
        self.assertEqual(argv[offset - 1:offset + 2],
            ["--ro-bind", "/fixture/requirements.toml", "/etc/codex/requirements.toml"])
        self.assertNotIn("/profile/requirements.toml", argv)

    def test_effective_closure_checks_actual_authored_immutable_overrides(self):
        arguments = probe.controller_arguments()
        config = {}
        for offset, argument in enumerate(arguments):
            if argument == "-c":
                config.update(tomllib.loads(arguments[offset + 1]))
        probe.verify_effective_closure(config)
        self.assertNotIn("model_provider", config)
        self.assertNotIn("forced_login_method", config)
        for field in ("notify", "mcp_servers", "features", "orchestrator"):
            changed = dict(config)
            del changed[field]
            with self.subTest(field=field), self.assertRaises(probe.ProbeRefused):
                probe.verify_effective_closure(changed)
        changed = dict(config, notify=["/bin/sh"])
        with self.assertRaises(probe.ProbeRefused):
            probe.verify_effective_closure(changed)

    def test_advertised_inventory_preserves_nested_tools(self):
        self.assertEqual(set(probe.tool_inventory([
            {"type": "namespace", "name": "functions", "tools": [
                {"type": "function", "name": "exec_command"},
                {"type": "custom", "name": "apply_patch"}]},
            {"type": "function", "name": "view_image"}])),
            {"functions.exec_command", "functions.apply_patch", "view_image"})

    def test_inventory_rejects_aliases_unknown_types_and_namespace_collisions(self):
        for tools in (
            [{"type": "web_search"}],
            [{"type": "function", "name": "skills.read"}],
            [{"type": "namespace", "name": "skills", "tools": []}],
            [{"type": "namespace", "name": "skills", "tools": None}],
            [{"type": "function", "name": "read"}] * 2,
            [{"type": "function", "name": "skills"},
             {"type": "namespace", "name": "skills", "tools": [
                 {"type": "function", "name": "read"}]}],
        ):
            with self.subTest(tools=tools), self.assertRaises(probe.ProbeRefused):
                probe.tool_inventory(tools)

    def test_inventory_validates_definitions_not_just_leaf_names(self):
        tools = [{"type": kind, "name": name} for name, kind in probe.EXPECTED_TOOL_TYPES.items()
                 if not name.startswith("skills.")]
        tools.append({"type": "namespace", "name": "skills", "tools": [
            {"type": "function", "name": "list"}, {"type": "function", "name": "read"}]})
        digest = probe.validate_inventory(tools, selected_skills=True)
        self.assertEqual(digest, probe.validate_inventory(list(reversed(tools)), selected_skills=True))
        tools[-1]["description"] = "namespace metadata must also be exact"
        self.assertNotEqual(digest, probe.validate_inventory(tools, selected_skills=True))
        digest = probe.validate_inventory(tools, selected_skills=True)
        tools[0]["parameters"] = {"properties": {"new_authority": {"type": "string"}}}
        self.assertNotEqual(digest, probe.validate_inventory(tools, selected_skills=True))
        with self.assertRaises(probe.ProbeRefused):
            probe.validate_inventory(tools)
        tools[-1]["name"] = "unreviewed"
        with self.assertRaises(probe.ProbeRefused):
            probe.validate_inventory(tools, selected_skills=True)

    def test_namespaced_call_keeps_explicit_namespace(self):
        call = probe.tool_call("read", {}, "skill-read", namespace="skills")
        self.assertEqual(call["namespace"], "skills")
        self.assertEqual(call["name"], "read")

    def test_provider_rejects_extra_requests_before_retention(self):
        provider = object.__new__(probe.ScriptedProvider)
        provider.calls, provider.requests = [], []
        provider.inventory_digest, provider.selected_skills = None, False
        request = {"tools": [{"name": name, "type": kind}
                              for name, kind in probe.EXPECTED_TOOL_TYPES.items()]}
        self.assertEqual(provider.retain_request(request), 0)
        for _ in range(10):
            with self.assertRaisesRegex(probe.ProbeRefused, "count exceeded"):
                provider.retain_request(request)
        self.assertEqual(len(provider.requests), 1)

    def test_provider_checks_definitions_on_later_requests(self):
        provider = object.__new__(probe.ScriptedProvider)
        provider.calls, provider.requests = [None, None], []
        provider.inventory_digest, provider.selected_skills = None, False
        request = {"tools": [{"name": name, "type": kind}
                              for name, kind in probe.EXPECTED_TOOL_TYPES.items()]}
        provider.retain_request(request)
        changed = probe.json.loads(probe.json.dumps(request))
        changed["tools"][0]["description"] = "changed after first request"
        with self.assertRaisesRegex(probe.ProbeRefused, "definitions changed"):
            provider.retain_request(changed)
        self.assertEqual(len(provider.requests), 1)

    def test_skill_read_uses_exact_observed_package_and_authority(self):
        catalog = {"skills": [{"name": "routing-fixture",
            "authority": {"kind": "executor", "id": "candidate-fixture"},
            "package": "observed-package", "main_resource": "observed-resource"}]}
        request = {"input": [{"type": "function_call_output", "call_id": "skill-list",
                              "output": probe.json.dumps(catalog)}]}
        item = {"call_id": "skill-read"}
        probe.prepare_call(item, request, Mock())
        arguments = probe.json.loads(item["arguments"])
        self.assertEqual(arguments["package"], "observed-package")
        self.assertEqual(arguments["resource"], "observed-resource")
        escaped = {"call_id": "skill-escape"}
        probe.prepare_call(escaped, request, Mock())
        self.assertEqual(probe.json.loads(escaped["arguments"])["resource"], "/workspace/outside-skill")
        catalog["skills"][0]["authority"]["id"] = "wrong-environment"
        request["input"][0]["output"] = probe.json.dumps(catalog)
        with self.assertRaisesRegex(probe.ProbeRefused, "wrong authority"):
            probe.prepare_call(item, request, Mock())

    def test_configuration_overlay_is_exact_and_flat(self):
        argv = probe.isolated_command(Path("/p/bwrap"), Path("/package"),
            Path("/candidate"), Path("/profile-source"), ["app-server"],
            protected_configs=("environments.toml",))
        offset = argv.index("/profile-source/environments.toml")
        self.assertEqual(argv[offset - 1:offset + 2],
            ["--ro-bind", "/profile-source/environments.toml", "/profile/environments.toml"])
        for invalid in ("../secret", "/secret", ".", "..", "a/b", "a\0b"):
            with self.assertRaises(probe.ProbeRefused):
                probe.isolated_command(Path("/p/bwrap"), Path("/package"),
                    Path("/candidate"), Path("/profile-source"), ["app-server"],
                    protected_configs=(invalid,))

    def test_configuration_protection_refuses_successful_mutation(self):
        with patch.object(probe.subprocess, "run", return_value=Mock(returncode=61)):
            with self.assertRaisesRegex(probe.ProbeRefused, "writable/removable"):
                probe.verify_configuration_protection(["bwrap", "--", "codex"])

    def test_tool_outputs_require_actual_returned_items(self):
        requests = [{"input": [
            {"type": "function_call", "call_id": "a", "arguments": "not evidence"},
            {"type": "function_call_output", "call_id": "a", "output": "remote"},
            {"type": "custom_tool_call_output", "call_id": "b", "output": "patch"}]}]
        self.assertEqual(probe.tool_outputs(requests), {"a": "remote", "b": "patch"})

    def test_stdin_uses_observed_session_id_and_refuses_missing_identity(self):
        item = {"call_id": "stdin-write"}
        with self.assertRaisesRegex(probe.ProbeRefused, "exact session id"):
            probe.prepare_call(item, {"input": []}, Mock())
        probe.prepare_call(item, {"input": [{"type": "function_call_output",
            "call_id": "stdin-start", "output": "Process running with session ID 428"}]}, Mock())
        self.assertEqual(probe.json.loads(item["arguments"])["session_id"], 428)

    def test_disconnect_stops_only_the_fixture_executor(self):
        executor = Mock()
        probe.prepare_call({"call_id": "disconnect"}, {}, executor)
        executor.stop.assert_called_once_with()

    def test_scripted_protocol_has_matching_completion_identity(self):
        item = probe.tool_call("apply_patch", "patch", "a")
        self.assertEqual(item["type"], "custom_tool_call")
        events = probe.response_events(item, 7)
        self.assertEqual(events[0]["response"]["id"], events[-1]["response"]["id"])
        self.assertEqual(events[1]["item"], item)
        self.assertEqual(events[-1]["response"]["usage"]["total_tokens"], 0)

    def test_fixture_image_has_valid_chunk_checksums(self):
        image = probe.fixture_image()
        self.assertEqual(image[:8], b"\x89PNG\r\n\x1a\n")
        offset = 8
        while offset < len(image):
            size = probe.struct.unpack(">I", image[offset:offset + 4])[0]
            payload = image[offset + 4:offset + 8 + size]
            checksum = probe.struct.unpack(">I", image[offset + 8 + size:offset + 12 + size])[0]
            self.assertEqual(checksum, probe.zlib.crc32(payload))
            offset += 12 + size
        self.assertEqual(offset, len(image))

    def test_fixture_process_refuses_closed_protocol(self):
        with probe.Process([sys.executable, "-I", "-c", "pass"]) as child:
            with self.assertRaisesRegex(probe.ProbeRefused, "closed"):
                child.line(time.monotonic() + 5)

    def test_fixture_process_enforces_deadline(self):
        with probe.Process([sys.executable, "-I", "-c", "import time; time.sleep(10)"]) as child:
            with self.assertRaisesRegex(probe.ProbeRefused, "deadline"):
                child.line(time.monotonic() + 0.05)

    def test_fixture_process_enforces_byte_bound(self):
        with patch.object(probe, "MAX_BYTES", 64):
            with probe.Process([sys.executable, "-I", "-c", "print('x'*2048)"]) as child:
                with self.assertRaisesRegex(probe.ProbeRefused, "byte bound"):
                    child.line(time.monotonic() + 5)

    def test_only_exact_owned_process_is_stopped_and_stop_is_repeatable(self):
        with probe.Process([sys.executable, "-I", "-c", "import time; time.sleep(10)"]) as child:
            child.stop()
            child.stop()
            self.assertIsNotNone(child.process.poll())


if __name__ == "__main__":
    unittest.main()
