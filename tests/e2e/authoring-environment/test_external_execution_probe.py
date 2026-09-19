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

    def test_advertised_inventory_preserves_nested_tools(self):
        self.assertEqual(probe.tool_names([
            {"type": "namespace", "name": "functions", "tools": [
                {"type": "function", "name": "exec_command"},
                {"type": "custom", "name": "apply_patch"}]},
            {"type": "function", "name": "view_image"}]),
            {"exec_command", "apply_patch", "view_image"})

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
