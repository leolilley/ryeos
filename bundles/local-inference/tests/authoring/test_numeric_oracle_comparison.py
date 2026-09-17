#!/usr/bin/env python3
"""Focused tests for consequence-free numeric candidate comparison."""

from __future__ import annotations

import importlib.util
import hashlib
import json
from pathlib import Path
import sys
import unittest


sys.dont_write_bytecode = True
SCRIPT = (
    Path(__file__).resolve().parents[2]
    / "authoring/compare_numeric_oracle.py"
)
BUNDLE = Path(__file__).resolve().parents[2]
EVIDENCE = (
    BUNDLE
    / "authoring/evidence/qwen3-4b-bf16-numeric-conformance-v1.json"
)
WORKER = BUNDLE / ".ai/workers/local-inference/lib/local-tinygrad"
SPEC = importlib.util.spec_from_file_location("compare_numeric_oracle", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def projection(entries: list[tuple[int, float]]) -> list[dict[str, object]]:
    return [
        {"token_id": token_id, "log_probability": probability}
        for token_id, probability in entries
    ]


class NumericOracleComparisonTests(unittest.TestCase):
    def setUp(self) -> None:
        self.thresholds = {
            "minimum_top16_token_overlap": 15,
            "maximum_matched_log_probability_absolute_error": 0.25,
            "maximum_matched_log_probability_mean_absolute_error": 0.05,
        }
        entries = [(token, -float(token)) for token in range(16)]
        self.reference = {
            "initial_top16_log_probabilities": projection(entries),
            "greedy_token_ids": list(range(8)),
        }

    def test_exact_result_passes(self) -> None:
        candidate = {
            "initial_top16_log_probabilities": projection(
                [(token, -float(token)) for token in range(16)]
            ),
            "greedy_token_ids": list(range(8)),
        }
        result = MODULE._compare_case(self.reference, candidate, self.thresholds)
        self.assertTrue(result["passed"])
        self.assertEqual(result["top16_overlap"], 16)
        self.assertEqual(result["maximum_matched_log_probability_absolute_error"], 0.0)

    def test_overlap_error_and_greedy_fail_closed(self) -> None:
        candidate = {
            "initial_top16_log_probabilities": projection(
                [(token, -float(token) - 0.3) for token in range(2, 18)]
            ),
            "greedy_token_ids": list(reversed(range(8))),
        }
        result = MODULE._compare_case(self.reference, candidate, self.thresholds)
        self.assertFalse(result["passed"])
        self.assertEqual(result["top16_overlap"], 14)
        self.assertFalse(result["checks"]["top1_token_matches"])
        self.assertFalse(result["checks"]["top16_overlap_sufficient"])
        self.assertFalse(result["checks"]["maximum_error_within_bound"])
        self.assertFalse(result["checks"]["greedy_tokens_match"])

    def test_reference_validation_rejects_duplicate_top_tokens(self) -> None:
        case = {
            "rendered_prompt_sha256": "0" * 64,
            "prompt_token_ids": [1],
            "initial_top16_log_probabilities": projection(
                [(0, 0.0), (0, -1.0)] + [(token, -float(token)) for token in range(2, 16)]
            ),
            "initial_top1_margin": 1.0,
            "greedy_token_ids": list(range(8)),
            "greedy_step_top1_margins": [1.0] * 8,
        }
        with self.assertRaisesRegex(ValueError, "top16 token is invalid"):
            MODULE._validate_reference_case("duplicate", case)

    def test_projection_breaks_logit_ties_by_lowest_token_id(self) -> None:
        top16, token, margin = MODULE._projection([0.0, 3.0, 3.0] + [-10.0] * 20)
        self.assertEqual(token, 1)
        self.assertEqual(top16[0]["token_id"], 1)
        self.assertEqual(top16[1]["token_id"], 2)
        self.assertEqual(margin, 0.0)

    def test_retained_evidence_binds_exact_inputs_without_activation(self) -> None:
        evidence = json.loads(EVIDENCE.read_bytes())
        self.assertEqual(
            set(evidence),
            {
                "schema",
                "scope",
                "consequences",
                "passed",
                "does_not_establish",
                "request",
                "model",
                "reference",
                "candidate",
                "cases",
                "retention",
            },
        )
        self.assertEqual(evidence["scope"], "numeric_conformance_only")
        self.assertEqual(evidence["consequences"], "none")
        self.assertTrue(evidence["passed"])
        self.assertIn("worker_or_provider_activation", evidence["does_not_establish"])
        self.assertEqual(
            evidence["candidate"]["execution_boundary"],
            {
                "interpreter": "modal_debian_slim_python_3.12",
                "interpreter_patch_and_binary_identity_retained": False,
                "admitted_runtime_archive_use": (
                    "musl_loader_and_libraries_for_admitted_compiler_only"
                ),
                "worker_runtime_abi_qualification": False,
            },
        )
        bound_paths = {
            "model.py": WORKER / "model.py",
            "model_contract.py": WORKER / "model_contract.py",
            "tokenizer.py": WORKER / "tokenizer.py",
            "compiler.py": WORKER / "compiler.py",
            "model-profiles/qwen3-4b.json": (
                WORKER / "model-profiles/qwen3-4b.json"
            ),
            "compare_numeric_oracle.py": SCRIPT,
        }
        for name, path in bound_paths.items():
            self.assertEqual(
                evidence["candidate"]["source_sha256"][name],
                hashlib.sha256(path.read_bytes()).hexdigest(),
            )
        request_path = BUNDLE / evidence["request"]["path"]
        self.assertEqual(
            evidence["request"]["sha256"],
            hashlib.sha256(request_path.read_bytes()).hexdigest(),
        )
        thresholds = json.loads(request_path.read_bytes())["comparison_thresholds"]
        for case in evidence["cases"].values():
            self.assertTrue(case["top1_token_matches"])
            self.assertTrue(case["greedy_tokens_match"])
            self.assertGreaterEqual(
                case["top16_overlap"], thresholds["minimum_top16_token_overlap"]
            )
            self.assertLessEqual(
                case["maximum_matched_log_probability_absolute_error"],
                thresholds["maximum_matched_log_probability_absolute_error"],
            )
            self.assertLessEqual(
                case["matched_log_probability_mean_absolute_error"],
                thresholds[
                    "maximum_matched_log_probability_mean_absolute_error"
                ],
            )


if __name__ == "__main__":
    unittest.main()
