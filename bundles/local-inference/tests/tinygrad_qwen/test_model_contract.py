#!/usr/bin/env python3
"""Offline tests for the exact Qwen model/shard admission contract."""

from __future__ import annotations

import json
import hashlib
from dataclasses import replace
from pathlib import Path
import sys
import tempfile
import unittest


WORKER = (
    Path(__file__).resolve().parents[2]
    / ".ai/workers/local-inference/lib/local-tinygrad"
)
sys.path.insert(0, str(WORKER))

from model_contract import (  # noqa: E402
    QWEN3_0_6B,
    QWEN3_4B,
    TensorContract,
    canonical_json,
    identify_model_contract,
    load_weight_map,
    validate_safetensors_header,
)
from tokenizer import render_chat  # noqa: E402


class QwenModelContractTests(unittest.TestCase):
    def test_exact_model_coordinates_and_tensor_totals_are_frozen(self) -> None:
        self.assertEqual(
            QWEN3_4B.upstream_revision,
            "3101254bbe4169895668a0e7653c3fd1f313576e",
        )
        self.assertEqual(QWEN3_4B.upstream_repository, "Qwen/Qwen3-4B")
        self.assertEqual(
            QWEN3_4B.sampler_id,
            "tinygrad-full-vocabulary-gumbel-max-v1",
        )
        self.assertEqual(
            QWEN3_4B.weight_map_sha256,
            "9cb9c16d3bc213510f48d3aca6726c9ef875b0ed5c7e2a952a4a4f61c30e0cc8",
        )
        self.assertEqual(len(QWEN3_0_6B.tensors), 311)
        self.assertEqual(len(QWEN3_4B.tensors), 398)
        self.assertEqual(
            sum(value.byte_length for value in QWEN3_0_6B.tensors.values()),
            QWEN3_0_6B.tensor_bytes,
        )
        self.assertEqual(
            sum(value.byte_length for value in QWEN3_4B.tensors.values()),
            8_044_936_192,
        )
        self.assertNotIn("lm_head.weight", QWEN3_4B.tensors)
        self.assertNotEqual(canonical_json({"value": True}), canonical_json({"value": 1}))
        self.assertNotEqual(canonical_json({"value": 1}), canonical_json({"value": 1.0}))

    def test_model_identity_requires_the_complete_exact_config(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "config.json").write_text(
                json.dumps(QWEN3_4B.config), encoding="utf-8"
            )
            self.assertEqual(identify_model_contract(root), QWEN3_4B)
            moved = dict(QWEN3_4B.config)
            moved["num_hidden_layers"] = 35
            (root / "config.json").write_text(json.dumps(moved), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "not an admitted exact contract"):
                identify_model_contract(root)
            wrong_type = dict(QWEN3_4B.config)
            wrong_type["attention_dropout"] = False
            (root / "config.json").write_text(
                json.dumps(wrong_type), encoding="utf-8"
            )
            with self.assertRaisesRegex(ValueError, "not an admitted exact contract"):
                identify_model_contract(root)
            (root / "config.json").write_text(
                '{"model_type":"qwen3","model_type":"qwen3"}', encoding="utf-8"
            )
            with self.assertRaisesRegex(ValueError, "duplicate JSON"):
                identify_model_contract(root)

    def test_qwen3_4b_template_and_tool_history_golden(self) -> None:
        arguments = '{ "row": 2, "column": 1 }'
        self.assertEqual(
            render_chat(
                [
                    {"role": "system", "content": "Be exact."},
                    {"role": "user", "content": "Inspect."},
                    {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [
                            {
                                "function": {
                                    "name": "inspect_grid",
                                    "arguments": arguments,
                                }
                            }
                        ],
                    },
                    {"role": "tool", "content": "blue"},
                ],
                [],
                enable_thinking=False,
            ),
            "<|im_start|>system\nBe exact.<|im_end|>\n"
            "<|im_start|>user\nInspect.<|im_end|>\n"
            "<|im_start|>assistant\n"
            f'<tool_call>\n{{"name": "inspect_grid", "arguments": {arguments}}}\n'
            "</tool_call><|im_end|>\n"
            "<|im_start|>user\n<tool_response>\nblue\n</tool_response><|im_end|>\n"
            "<|im_start|>assistant\n<think>\n\n</think>\n\n",
        )

    def test_sharded_index_is_exact_and_canonical(self) -> None:
        tensors = sorted(QWEN3_4B.tensors)
        weight_map = {
            name: QWEN3_4B.shard_names[index % len(QWEN3_4B.shard_names)]
            for index, name in enumerate(tensors)
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            index_path = root / "model.safetensors.index.json"

            def write(value: object) -> None:
                index_path.write_text(json.dumps(value), encoding="utf-8")

            exact = {
                "metadata": {"total_size": QWEN3_4B.tensor_bytes},
                "weight_map": weight_map,
            }
            write(exact)
            synthetic_digest = hashlib.sha256(
                json.dumps(
                    weight_map, ensure_ascii=False, sort_keys=True, separators=(",", ":")
                ).encode("utf-8")
            ).hexdigest()
            synthetic_contract = replace(
                QWEN3_4B, weight_map_sha256=synthetic_digest
            )
            self.assertEqual(load_weight_map(root, synthetic_contract), weight_map)

            with self.assertRaisesRegex(ValueError, "assignment changed"):
                load_weight_map(root, QWEN3_4B)

            missing = json.loads(json.dumps(exact))
            missing["weight_map"].pop(tensors[0])
            write(missing)
            with self.assertRaisesRegex(ValueError, "tensor set changed"):
                load_weight_map(root, synthetic_contract)

            escaped = json.loads(json.dumps(exact))
            escaped["weight_map"][tensors[0]] = "../model-00001-of-00003.safetensors"
            write(escaped)
            with self.assertRaisesRegex(ValueError, "non-canonical shard"):
                load_weight_map(root, synthetic_contract)

            extra_shard = json.loads(json.dumps(exact))
            extra_shard["weight_map"][tensors[0]] = "model-00004-of-00004.safetensors"
            write(extra_shard)
            with self.assertRaisesRegex(ValueError, "shard set changed"):
                load_weight_map(root, synthetic_contract)

            wrong_total = json.loads(json.dumps(exact))
            wrong_total["metadata"]["total_size"] += 2
            write(wrong_total)
            with self.assertRaisesRegex(ValueError, "total size changed"):
                load_weight_map(root, synthetic_contract)

            wrong_type = json.loads(json.dumps(exact))
            wrong_type["metadata"]["total_size"] = float(QWEN3_4B.tensor_bytes)
            write(wrong_type)
            with self.assertRaisesRegex(ValueError, "total size changed"):
                load_weight_map(root, synthetic_contract)

    def test_header_rejects_overlap_gaps_extras_shape_and_dtype(self) -> None:
        expected = {
            "a": TensorContract((2,)),
            "b": TensorContract((3,)),
        }

        def entry(shape: list[int], begin: int, end: int, dtype: str = "BF16") -> dict:
            return {"dtype": dtype, "shape": shape, "data_offsets": [begin, end]}

        exact = {"a": entry([2], 0, 4), "b": entry([3], 4, 10)}
        self.assertEqual(
            set(
                validate_safetensors_header(
                    shard_name="model-00001-of-00003.safetensors",
                    header=exact,
                    data_bytes=10,
                    expected=expected,
                )
            ),
            {"a", "b"},
        )
        cases = {
            "overlaps": ({"a": entry([2], 0, 4), "b": entry([3], 2, 8)}, 8),
            "unindexed bytes": ({"a": entry([2], 0, 4), "b": entry([3], 6, 12)}, 12),
            "trailing": (exact, 12),
            "tensor set": ({**exact, "c": entry([1], 10, 12)}, 12),
            "shape or dtype": ({"a": entry([2], 0, 4, "F16"), "b": exact["b"]}, 10),
            "typed shape": ({"a": entry([True], 0, 4), "b": exact["b"]}, 10),
        }
        for reason, (header, size) in cases.items():
            with self.subTest(reason=reason):
                expected_reason = "shape or dtype" if reason == "typed shape" else reason
                with self.assertRaisesRegex(ValueError, expected_reason):
                    validate_safetensors_header(
                        shard_name="model-00001-of-00003.safetensors",
                        header=header,
                        data_bytes=size,
                        expected=expected,
                    )


if __name__ == "__main__":
    unittest.main()
