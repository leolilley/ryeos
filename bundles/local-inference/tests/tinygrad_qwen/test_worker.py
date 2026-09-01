#!/usr/bin/env python3
"""Targeted contract tests for the admitted tinygrad/Qwen worker."""

from __future__ import annotations

import hashlib
import json
import os
import struct
import sys
import threading
import unittest
import unicodedata
from pathlib import Path

# The test imports the exact signed worker source from its staged workspace.
# Never mutate that source closure with interpreter-owned cache artifacts.
sys.dont_write_bytecode = True


WORKSPACE = Path(os.environ["RYEOS_LOCAL_WORKER_TEST_WORKSPACE"]).resolve(strict=True)
RUN_MODEL_GOLDENS = os.environ.get("RYEOS_RUN_MODEL_GOLDENS") == "1"
os.environ["REGEN"] = "1"
os.environ["DEVICE"] = "HOST-SHOULD-NOT-SELECT-A-BACKEND"
os.chdir(WORKSPACE)
sys.path[:0] = [str(WORKSPACE / "worker"), str(WORKSPACE / "tinygrad")]

from session import WORKER_ROOT, OutputRouter, Worker, _read_frame, _validate_request  # noqa: E402
from model import QwenModel  # noqa: E402
from tinygrad import Tensor  # noqa: E402
from tokenizer import QwenTokenizer, render_chat  # noqa: E402


class WorkerContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.model_root = WORKSPACE / "model"
        cls.tokenizer = QwenTokenizer(cls.model_root)

    def test_worker_source_root_is_derived_from_the_admitted_entrypoint(self) -> None:
        self.assertEqual(WORKER_ROOT, Path(__import__("session").__file__).resolve().parent)
        self.assertIn(WORKSPACE, WORKER_ROOT.parents)

    def test_tokenizer_matches_independent_reference_ids(self) -> None:
        cases = {
            "hello": [14990],
            "Hello, world!": [9707, 11, 1879, 0],
            "café cafe\u0301": [924, 58858, 51950],
            "你好, 世界": [108386, 11, 220, 99489],
            "line one\nline two\n": [1056, 825, 198, 1056, 1378, 198],
            "<|im_start|>user\nReply OK.<|im_end|>\n<|im_start|>assistant\n": [
                151644,
                872,
                198,
                20841,
                10402,
                13,
                151645,
                198,
                151644,
                77091,
                198,
            ],
            '<think>reason</think><tool_call>{"name":"x","arguments":{}}</tool_call>': [
                151667,
                19895,
                151668,
                151657,
                4913,
                606,
                3252,
                87,
                2198,
                16370,
                788,
                90,
                3417,
                151658,
            ],
        }
        for text, expected in cases.items():
            with self.subTest(text=text):
                self.assertEqual(self.tokenizer.encode(text), expected)
                self.assertEqual(self.tokenizer.decode(expected), unicodedata.normalize("NFC", text))

    def test_chat_rendering_matches_the_pinned_template_contract(self) -> None:
        rendered = render_chat([{"role": "user", "content": "Reply OK."}], [])
        self.assertEqual(
            rendered,
            "<|im_start|>user\nReply OK.<|im_end|>\n<|im_start|>assistant\n",
        )
        non_thinking = render_chat(
            [{"role": "user", "content": "Reply OK."}], [], enable_thinking=False
        )
        self.assertEqual(
            non_thinking,
            "<|im_start|>user\nReply OK.<|im_end|>\n"
            "<|im_start|>assistant\n<think>\n\n</think>\n\n",
        )
        rendered_with_tools = render_chat(
            [
                {"role": "system", "content": "Be exact."},
                {"role": "user", "content": "Use it."},
            ],
            [
                {
                    "type": "function",
                    "function": {
                        "name": "probe",
                        "description": "Probe.",
                        "parameters": {"type": "object"},
                    },
                }
            ],
        )
        self.assertIn("# Tools\n", rendered_with_tools)
        self.assertIn('<tools>\n{"type":"function"', rendered_with_tools)
        self.assertTrue(rendered_with_tools.endswith("<|im_start|>assistant\n"))

    def test_rust_cancel_frame_fixture_is_accepted(self) -> None:
        payload = (
            b'{"protocol":"ryeos.persistent-session","version":1,'
            b'"kind":"cancel","request_id":"cancel-fixture","body":null}'
        )
        read_fd, write_fd = os.pipe()
        try:
            os.write(write_fd, struct.pack(">I", len(payload)) + payload)
            self.assertEqual(
                _read_frame(read_fd),
                {
                    "protocol": "ryeos.persistent-session",
                    "version": 1,
                    "kind": "cancel",
                    "request_id": "cancel-fixture",
                    "body": None,
                },
            )
        finally:
            os.close(read_fd)
            os.close(write_fd)

    def test_frame_parser_rejects_duplicate_members_and_non_finite_numbers(self) -> None:
        malformed = [
            (
                b'{"protocol":"ryeos.persistent-session",'
                b'"protocol":"ryeos.persistent-session","version":1,'
                b'"kind":"cancel","request_id":"x","body":null}'
            ),
            (
                b'{"protocol":"ryeos.persistent-session","version":1,'
                b'"kind":"request","request_id":"x","body":{"value":NaN}}'
            ),
        ]
        for payload in malformed:
            with self.subTest(payload=payload):
                read_fd, write_fd = os.pipe()
                try:
                    os.write(write_fd, struct.pack(">I", len(payload)) + payload)
                    with self.assertRaises(ValueError):
                        _read_frame(read_fd)
                finally:
                    os.close(read_fd)
                    os.close(write_fd)

    def test_reasoning_and_content_routing_survives_token_boundaries(self) -> None:
        router = OutputRouter()
        routed: list[tuple[str, str]] = []
        for chunk in ("<thi", "nk>reas", "on</think>ans", "wer"):
            routed.extend(router.route(chunk))
        routed.extend(router.route("", final=True))
        self.assertEqual(
            routed,
            [("reasoning", "reas"), ("reasoning", "on"), ("content", "ans"), ("content", "wer")],
        )

    def test_request_bounds_fail_before_inference(self) -> None:
        request = {
            "model": "qwen3-0.6b",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [],
            "stream": True,
            "stream_options": {"include_usage": True},
            "max_tokens": 256,
            "temperature": 0.0,
            "seed": 0,
        }
        def outer_for(value: dict[str, object]) -> dict[str, object]:
            request_body = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
            return {
                "request_body": request_body,
                "request_body_sha256": hashlib.sha256(request_body.encode("utf-8")).hexdigest(),
                "requested_output_ceiling": 32768,
            }

        outer = outer_for(request)
        _, output_limit, temperature, seed = _validate_request(outer)
        self.assertEqual((output_limit, temperature, seed), (256, 0.0, 0))
        tampered = dict(outer)
        tampered["request_body"] = str(tampered["request_body"]) + " "
        with self.assertRaisesRegex(ValueError, "admitted digest"):
            _validate_request(tampered)
        request["max_tokens"] = 257
        with self.assertRaisesRegex(ValueError, "output limit"):
            _validate_request(outer_for(request))
        request["max_tokens"] = 1
        request["temperature"] = float("nan")
        with self.assertRaisesRegex(ValueError, "non-finite JSON number"):
            _validate_request(outer_for(request))

    def test_kernel_lowering_uses_the_admitted_compiler(self) -> None:
        value = (Tensor([1.0, 2.0]) + Tensor([3.0, 4.0])).realize()
        self.assertEqual(value.shape, (2,))
        self.assertEqual(value.device, "CPU")

    def test_mutable_tinygrad_cache_is_neither_read_nor_written(self) -> None:
        cache_db = Path(os.environ["XDG_CACHE_HOME"]) / "tinygrad" / "cache.db"
        cache_db.parent.mkdir(parents=True, exist_ok=True)
        poisoned = b"not-a-sqlite-database\x00untrusted-cache-sentinel"
        cache_db.write_bytes(poisoned)

        value = (Tensor([2.0]) * Tensor([4.0])).realize()

        self.assertEqual(value.shape, (1,))
        self.assertEqual(cache_db.read_bytes(), poisoned)

    def test_worker_discards_host_tinygrad_controls(self) -> None:
        self.assertNotIn("REGEN", os.environ)
        self.assertNotIn("DEVICE", os.environ)
        self.assertEqual(os.environ.get("DEV"), "CPU")

    @unittest.skipUnless(
        RUN_MODEL_GOLDENS,
        "enable the targeted model golden explicitly",
    )
    def test_model_mapping_is_complete_and_strict(self) -> None:
        model = QwenModel(self.model_root)
        self.assertEqual(len(model._mapped.tensors), 311)
        self.assertEqual(len(model._model.blk), 28)

    @unittest.skipUnless(
        RUN_MODEL_GOLDENS,
        "enable the targeted model golden explicitly",
    )
    def test_generation_repeatability_and_kv_cache_equivalence(self) -> None:
        prompt = self.tokenizer.encode(
            render_chat([{"role": "user", "content": "Reply OK."}], [])
        )
        model = QwenModel(self.model_root)
        embedded = model._model.token_embd(Tensor([prompt])).float()
        for block in model._model.blk:
            embedded = block(embedded, 0)
        logits = model._model.output(model._model.output_norm(embedded))[:, -1, :].realize()
        independent_reference = {
            151667: 31.75,
            151668: 21.375,
            151644: 21.375,
            151645: 20.625,
            33137: 20.0,
            2784: 19.0,
            151657: 18.75,
            30076: 18.75,
        }
        for token, expected in independent_reference.items():
            with self.subTest(token=token):
                self.assertAlmostEqual(
                    float(logits[0, token].item()), expected, delta=0.15
                )
        self.assertEqual(int(logits.argmax().item()), 151667)

        generation = model.generate(prompt, 2, 0.0, 0)
        first = next(generation)
        cached_second = next(generation)
        self.assertEqual(first, 151667)
        # Independent Transformers full-prefix reference for prompt + first.
        self.assertEqual(cached_second, 198)

        tool_prompt = self.tokenizer.encode(
            render_chat(
                [{"role": "user", "content": "Call read with an empty object now."}],
                [
                    {
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": description,
                            "parameters": {
                                "type": "object",
                                "properties": {},
                                "additionalProperties": False,
                            },
                        },
                    }
                    for name, description in (
                        ("read", "Read the fixed admitted input. Takes no arguments."),
                        ("mutate", "Create the fixed candidate. Takes no arguments."),
                        ("verify", "Verify the input and candidate. Takes no arguments."),
                    )
                ],
                enable_thinking=False,
            )
        )
        # Six independent fresh-Transformer full-prefix evaluations. A plain
        # integer start position can accidentally pass the two-token check but
        # lets TinyJit reuse the first rollout's KV position thereafter.
        self.assertEqual(
            list(model.generate(tool_prompt, 6, 0.0, 0)),
            [151657, 198, 4913, 606, 788, 330],
        )

    @unittest.skipUnless(
        RUN_MODEL_GOLDENS,
        "enable the targeted model golden explicitly",
    )
    def test_supervisor_isolates_generation_state_between_requests(self) -> None:
        def request(enable_thinking: bool, output_limit: int) -> dict[str, object]:
            body = {
                "model": "qwen3-0.6b",
                "messages": [
                    {
                        "role": "user",
                        "content": "Reply with exactly `OK` and nothing else.\n",
                    }
                ],
                "tools": [],
                "stream": True,
                "stream_options": {"include_usage": True},
                "max_tokens": output_limit,
                "temperature": 0.0,
                "seed": 0,
                "enable_thinking": enable_thinking,
            }
            request_body = json.dumps(body, ensure_ascii=False, separators=(",", ":"))
            return {
                "request_body": request_body,
                "request_body_sha256": hashlib.sha256(request_body.encode("utf-8")).hexdigest(),
                "requested_output_ceiling": 64,
            }

        supervisor = Worker(load_model=True)
        supervisor.execute("thinking", request(True, 8), threading.Event(), lambda _delta: None)
        result = supervisor.execute(
            "non-thinking", request(False, 6), threading.Event(), lambda _delta: None
        )
        self.assertEqual(result["answer"]["message"]["content"], "OK")
        self.assertEqual(result["answer"]["finish_reason"], "stop")

    @unittest.skipUnless(
        RUN_MODEL_GOLDENS,
        "enable the targeted model golden explicitly",
    )
    def test_seeded_sampling_is_repeatable_with_fresh_generation_state(self) -> None:
        prompt = self.tokenizer.encode(
            render_chat([{"role": "user", "content": "Name one color."}], [])
        )
        model = QwenModel(self.model_root)
        first = list(model.generate(prompt, 6, 0.7, 4242))
        second = list(model.generate(prompt, 6, 0.7, 4242))
        self.assertEqual(first, second)


if __name__ == "__main__":
    unittest.main()
