"""Refusal tests use an explicit synthetic encoder; no tokenizer-fit claim."""
import hashlib
import importlib.util
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


AUTHORING = Path(__file__).resolve().parents[2] / "authoring"
spec = importlib.util.spec_from_file_location("prompt_packing", AUTHORING / "check_prompt_packing.py")
packing = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packing)


class PromptPackingTests(unittest.TestCase):
    def request(self, name="one"):
        return {"id": name, "messages": [{"role": "user", "content": "x"}],
                "tools": [], "enable_thinking": False}

    def check(self, requests, **changes):
        arguments = dict(encode=lambda text: [ord(char) for char in text],
                         render=lambda messages, tools, thinking: "template:" + messages[0]["content"],
                         input_limit=20, output_limit=5, context_limit=30, profile_output_limit=10)
        arguments.update(changes)
        return packing.pack_requests(requests, **arguments)

    def test_full_rendering_overhead_can_refuse_a_small_raw_message(self):
        row = self.check([self.request()], input_limit=3)[0]
        self.assertEqual(row["rendered_input_tokens"], 10)
        self.assertEqual(row["refusal"], "rendered_input_exceeds_limit")
        self.assertFalse(row["fits"])

    def test_output_reservation_can_refuse_an_input_that_fits(self):
        row = self.check([self.request()], input_limit=10, context_limit=12)[0]
        self.assertEqual(row["refusal"], "input_plus_output_exceeds_context")

    def test_every_assigned_request_is_preserved_without_truncation(self):
        long = self.request("long")
        long["messages"][0]["content"] = "x" * 40
        rows = self.check([long, self.request("short")])
        self.assertEqual([row["id"] for row in rows], ["long", "short"])
        self.assertEqual(rows[0]["rendered_input_tokens"], 49)
        self.assertFalse(rows[0]["fits"])
        self.assertTrue(rows[1]["fits"])

    def test_invalid_or_profile_exceeding_limits_are_refused(self):
        for change in [{"input_limit": True}, {"output_limit": 0}, {"output_limit": 11},
                       {"input_limit": 31}]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.check([self.request()], **change)

    def test_duplicate_or_incomplete_requests_are_refused(self):
        for requests in [[self.request(), self.request()], [{"id": "one"}], []]:
            with self.subTest(requests=requests), self.assertRaises(ValueError):
                self.check(requests)

    def test_metadata_drift_and_symlink_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            expected = {}
            for name in packing.METADATA_FILES:
                (root / name).write_bytes(b"{}")
                expected[name] = hashlib.sha256(b"{}").hexdigest()
            self.assertEqual(len(packing.verify_metadata(root, expected)), 4)
            (root / "tokenizer.json").write_bytes(b"[]")
            with self.assertRaises(ValueError):
                packing.verify_metadata(root, expected)
            (root / "tokenizer.json").unlink()
            (root / "tokenizer.json").symlink_to(root / "config.json")
            with self.assertRaises(OSError):
                packing.verify_metadata(root, expected)

    def test_real_renderer_includes_generation_and_thinking_markers(self):
        module = packing.worker_tokenizer_module()
        observed = []

        def synthetic_encode(text):
            observed.append(text)
            return list(range(len(text)))

        rows = self.check([self.request()], encode=synthetic_encode, render=module.render_chat,
                          input_limit=1000, context_limit=2000)
        self.assertIn("<|im_start|>assistant\n", observed[0])
        self.assertIn("<think>\n\n</think>\n\n", observed[0])
        self.assertEqual(rows[0]["rendered_sha256"], hashlib.sha256(observed[0].encode()).hexdigest())

    def test_absent_metadata_cli_returns_refusal_without_token_counts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            requests = root / "requests.json"
            requests.write_text(json.dumps([self.request()]))
            arguments = ["check_prompt_packing.py", "--metadata-root", str(root / "missing"),
                         "--requests", str(requests), "--profile", "qwen3-0.6b",
                         "--input-limit", "1536", "--output-limit", "256"]
            output = io.StringIO()
            with patch.object(packing.sys, "argv", arguments), contextlib.redirect_stderr(output):
                self.assertEqual(packing.cli(), 2)
            result = json.loads(output.getvalue())
            self.assertFalse(result["actual_token_counts_available"])
            self.assertEqual(result["model_or_device_contacts"], 0)
            self.assertNotIn("rows", result)


if __name__ == "__main__":
    unittest.main()
