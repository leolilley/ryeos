#!/usr/bin/env python3
"""Validate the pre-observation Qwen3-4B numeric-oracle request."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import sys
import unittest


HERE = Path(__file__).resolve().parent
BUNDLE = HERE.parent.parent
REQUEST = (
    BUNDLE
    / "authoring/contracts/qwen3-4b-bf16-numeric-oracle-request-v1.json"
)
SOURCE_CONTRACT = (
    BUNDLE / "authoring/contracts/qwen3-4b-bf16-model-source-v1.json"
)
WORKER_SOURCE = BUNDLE / ".ai/workers/local-inference/lib/local-tinygrad"
sys.path.insert(0, str(WORKER_SOURCE))
from model_contract import QWEN3_4B  # noqa: E402
from tokenizer import render_chat  # noqa: E402


def strict_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, member in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object member: {key}")
        value[key] = member
    return value


def reject_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number is forbidden: {value}")


def load_json(path: Path) -> dict[str, object]:
    value = json.loads(
        path.read_bytes(),
        object_pairs_hook=strict_object,
        parse_constant=reject_constant,
    )
    if not isinstance(value, dict):
        raise ValueError(f"{path.name} is not a JSON object")
    return value


class NumericOracleRequestTests(unittest.TestCase):
    def test_strict_json_rejects_duplicate_and_non_finite_members(self) -> None:
        with self.assertRaisesRegex(ValueError, "duplicate JSON"):
            json.loads('{"a":1,"a":2}', object_pairs_hook=strict_object)
        with self.assertRaisesRegex(ValueError, "non-finite"):
            json.loads('{"a":NaN}', parse_constant=reject_constant)

    def test_request_is_exactly_bound_before_candidate_observation(self) -> None:
        request = load_json(REQUEST)
        self.assertEqual(
            set(request),
            {
                "schema",
                "scope",
                "model",
                "reference",
                "procedure",
                "measurement",
                "comparison_thresholds",
                "result_requirements",
                "cases",
            },
        )
        self.assertEqual(
            request["schema"], "ryeos.local_inference_numeric_oracle_request.v1"
        )
        source_bytes = SOURCE_CONTRACT.read_bytes()
        model = request["model"]
        self.assertIsInstance(model, dict)
        self.assertEqual(
            model,
            {
                "model_id": QWEN3_4B.model_id,
                "repository": QWEN3_4B.upstream_repository,
                "revision": QWEN3_4B.upstream_revision,
                "source_contract_sha256": hashlib.sha256(source_bytes).hexdigest(),
                "source_dtype": "bfloat16",
                "candidate_sampler_id": QWEN3_4B.sampler_id,
            },
        )
        self.assertNotIn("results", request)
        self.assertNotIn("candidate", request)
        self.assertEqual(
            request["scope"],
            {
                "establishes": "numeric_conformance_only",
                "consequences": "none",
                "does_not_establish": [
                    "hardware_qualification",
                    "runtime_or_isolation_qualification",
                    "worker_or_provider_activation",
                    "artifact_publication",
                    "arc_candidate_acceptance",
                ],
            },
        )

    def test_reference_is_independent_closed_and_offline(self) -> None:
        reference = load_json(REQUEST)["reference"]
        self.assertEqual(
            reference,
            {
                "implementation": (
                    "transformers.models.qwen3.modeling_qwen3.Qwen3ForCausalLM"
                ),
                "tokenizer_loader": "transformers.AutoTokenizer",
                "model_loader": "Qwen3ForCausalLM.from_pretrained",
                "python_version": "3.11.11",
                "torch_version": "2.7.0",
                "transformers_version": "4.51.0",
                "tokenizers_version": "0.21.1",
                "safetensors_version": "0.5.3",
                "device": "cpu",
                "source_dtype": "bfloat16",
                "compute_dtype": "float32",
                "attention_implementation": "eager",
                "trust_remote_code": False,
                "network": "disabled",
                "deterministic_algorithms": True,
                "torch_num_threads": 1,
                "torch_num_interop_threads": 1,
            },
        )
        self.assertNotIn("tinygrad", json.dumps(reference).lower())

    def test_comparison_policy_is_frozen_and_non_permissive(self) -> None:
        request = load_json(REQUEST)
        self.assertEqual(
            request["measurement"],
            {
                "prompt_token_ids": "exact",
                "logit_position": "last_prompt_token",
                "logit_projection": "top_16_log_softmax_float32",
                "greedy_generation_tokens": 8,
                "temperature": 0.0,
                "seed": 0,
            },
        )
        self.assertEqual(
            request["procedure"],
            {
                "tokenizer_load": (
                    "AutoTokenizer.from_pretrained(model_root, local_files_only=True, "
                    "trust_remote_code=False, use_fast=True)"
                ),
                "reference_render": (
                    "tokenizer.apply_chat_template(messages, tools=tools_or_none, "
                    "tokenize=False, add_generation_prompt=True, "
                    "enable_thinking=case.enable_thinking)"
                ),
                "render_requirement": (
                    "reference_render_utf8_must_equal_case_rendered_prompt_utf8"
                ),
                "tokenize": (
                    "tokenizer.encode(case.rendered_prompt_utf8, "
                    "add_special_tokens=False)"
                ),
                "tokenize_requirements": {
                    "normalization": "tokenizer_owned",
                    "padding": False,
                    "truncation": False,
                    "attention_mask": "all_ones",
                },
                "model_load": (
                    "Qwen3ForCausalLM.from_pretrained(model_root, "
                    "local_files_only=True, trust_remote_code=False, "
                    "torch_dtype=torch.float32, attn_implementation='eager', "
                    "low_cpu_mem_usage=False)"
                ),
                "model_mode": "eval_plus_torch_inference_mode",
                "forward": "full_prefix_recompute_use_cache_false",
                "position_ids": (
                    "transformers_default_from_all_ones_attention_mask"
                ),
                "initial_projection": (
                    "float32_log_softmax_of_logits_at_last_prompt_token"
                ),
                "top16_order": (
                    "descending_log_probability_then_ascending_token_id"
                ),
                "greedy": {
                    "steps": 8,
                    "stop_on_eos": False,
                    "include_first_predicted_token": True,
                    "selection": "maximum_logit_then_lowest_token_id",
                    "next_step": (
                        "append_selected_token_and_recompute_complete_prefix_without_cache"
                    ),
                    "record": (
                        "selected_token_and_top1_minus_top2_logit_margin_for_every_step"
                    ),
                },
                "sampling_scope": (
                    "temperature_zero_only; seeded_gumbel_repeatability_is_a_separate_activation_gate"
                ),
            },
        )
        self.assertEqual(
            request["comparison_thresholds"],
            {
                "minimum_reference_top1_margin": 0.5,
                "reference_margin_scope": "initial_projection_and_every_greedy_step",
                "insufficient_reference_margin": (
                    "invalidate_oracle_before_candidate_observation"
                ),
                "top1_token_must_match": True,
                "minimum_top16_token_overlap": 15,
                "maximum_matched_log_probability_absolute_error": 0.25,
                "maximum_matched_log_probability_mean_absolute_error": 0.05,
                "greedy_tokens_must_match": True,
            },
        )
        self.assertEqual(
            request["result_requirements"],
            {
                "bind_request": "sha256_of_exact_request_bytes",
                "content_identity_contract": {
                    "manifest_kind": "external_large_content_manifest",
                    "manifest_schema": "ryeos.external_content.large.v2",
                    "digest": (
                        "lowercase_sha256_of_lillux_canonical_json_manifest_object"
                    ),
                },
                "bind_model_tree": "external_large_content_manifest_digest",
                "bind_reference_environment": (
                    "external_large_content_manifest_digest"
                ),
                "reference_environment_manifest_must_include": [
                    "interpreter_and_package_artifacts",
                    "native_libraries",
                    "operating_system_and_architecture",
                    "torch_build_config",
                    "device_identity",
                    "thread_and_determinism_settings",
                ],
                "case_result_must_include": [
                    "rendered_prompt_sha256",
                    "prompt_token_ids",
                    "initial_top16_log_probabilities",
                    "initial_top1_margin",
                    "greedy_token_ids",
                    "greedy_step_top1_margins",
                ],
                "case_result_shape": {
                    "object_members": "exactly_case_result_must_include",
                    "rendered_prompt_sha256": "64_lowercase_hex_characters",
                    "prompt_token_ids": (
                        "nonempty_array_of_nonnegative_integers"
                    ),
                    "initial_top16_log_probabilities": {
                        "representation": "array_of_exactly_16_objects",
                        "entry_members": ["token_id", "log_probability"],
                        "token_id": "unique_nonnegative_integer",
                        "log_probability": "finite_json_number",
                        "order": (
                            "descending_log_probability_then_ascending_token_id"
                        ),
                    },
                    "initial_top1_margin": "finite_nonnegative_json_number",
                    "greedy_token_ids": (
                        "array_of_exactly_8_nonnegative_integers"
                    ),
                    "greedy_step_top1_margins": (
                        "array_of_exactly_8_finite_nonnegative_json_numbers"
                    ),
                },
                "comparison_semantics": {
                    "matched_tokens": "intersection_by_exact_token_id",
                    "duplicate_token_ids": "invalidate_result",
                    "top16_overlap": "cardinality_of_matched_tokens",
                    "matched_absolute_error": (
                        "absolute_difference_of_log_probability_for_each_matched_token_id"
                    ),
                    "matched_mean_absolute_error": (
                        "sum_of_matched_absolute_errors_divided_by_top16_overlap"
                    ),
                    "unmatched_tokens": (
                        "excluded_from_error_aggregates_but_count_against_overlap"
                    ),
                    "nonfinite_numbers": "invalidate_result",
                },
            },
        )

    def test_cases_match_the_admitted_renderer_exactly(self) -> None:
        cases = load_json(REQUEST)["cases"]
        self.assertIsInstance(cases, list)
        self.assertEqual(
            [case["id"] for case in cases],
            [
                "plain_exact_reply",
                "thinking_tool_request",
                "tool_response_continuation",
            ],
        )
        expected_messages = {
            "plain_exact_reply": [
                {
                    "role": "system",
                    "content": "You are a deterministic qualification probe.",
                },
                {"role": "user", "content": "Reply with exactly QUALIFIED."},
            ],
            "thinking_tool_request": [
                {"role": "system", "content": "Use the supplied tool exactly once."},
                {"role": "user", "content": "Inspect row 2, column 1."},
            ],
            "tool_response_continuation": [
                {"role": "system", "content": "Use the supplied tool exactly once."},
                {"role": "user", "content": "Inspect row 2, column 1."},
                {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "function": {
                                "name": "inspect_grid",
                                "arguments": '{"row":2,"column":1}',
                            }
                        }
                    ],
                },
                {"role": "tool", "content": "blue"},
            ],
        }
        exact_tool = {
            "type": "function",
            "function": {
                "name": "inspect_grid",
                "description": "Read one grid cell.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "row": {"type": "integer"},
                        "column": {"type": "integer"},
                    },
                    "required": ["row", "column"],
                    "additionalProperties": False,
                },
            },
        }
        for case in cases:
            with self.subTest(case=case["id"]):
                self.assertEqual(
                    set(case),
                    {
                        "id",
                        "messages",
                        "tools",
                        "enable_thinking",
                        "rendered_prompt_utf8",
                        "rendered_prompt_bytes",
                        "rendered_prompt_sha256",
                    },
                )
                self.assertIsInstance(case["enable_thinking"], bool)
                self.assertEqual(case["messages"], expected_messages[case["id"]])
                self.assertEqual(
                    case["tools"],
                    [] if case["id"] == "plain_exact_reply" else [exact_tool],
                )
                for message in case["messages"]:
                    for call in message.get("tool_calls", []):
                        arguments = json.loads(
                            call["function"]["arguments"],
                            object_pairs_hook=strict_object,
                            parse_constant=reject_constant,
                        )
                        self.assertEqual(arguments, {"row": 2, "column": 1})
                rendered = render_chat(
                    case["messages"],
                    case["tools"],
                    enable_thinking=case["enable_thinking"],
                ).encode("utf-8")
                self.assertEqual(rendered, case["rendered_prompt_utf8"].encode("utf-8"))
                self.assertEqual(len(rendered), case["rendered_prompt_bytes"])
                self.assertEqual(
                    hashlib.sha256(rendered).hexdigest(),
                    case["rendered_prompt_sha256"],
                )


if __name__ == "__main__":
    unittest.main()
