#!/usr/bin/env python3
"""Compare one exact local Qwen candidate with a frozen independent oracle.

This authoring utility has no activation consequence. It observes the admitted
model through the bundle-owned full-prefix logits projection and emits bounded,
typed comparison evidence for a later, separate qualification decision.
"""

from __future__ import annotations

import argparse
import gc
import hashlib
import json
import math
from pathlib import Path
import sys
import time
from typing import Any


MAX_JSON_BYTES = 8 * 1024 * 1024
EXPECTED_ORACLE_SCHEMA = "ryeos.local_inference_numeric_oracle.v1"
RESULT_SCHEMA = "ryeos.local_inference_numeric_candidate_comparison.v1"


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON object member: {key}")
        result[key] = value
    return result


def _reject_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number is forbidden: {value}")


def _load_json(path: Path) -> tuple[bytes, dict[str, Any]]:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"{path} is not an ordinary file")
    encoded = path.read_bytes()
    if not encoded or len(encoded) > MAX_JSON_BYTES:
        raise ValueError(f"{path} is outside the admitted JSON bound")
    value = json.loads(
        encoded,
        object_pairs_hook=_strict_object,
        parse_constant=_reject_constant,
    )
    if not isinstance(value, dict):
        raise ValueError(f"{path} is not a JSON object")
    return encoded, value


def _finite_number(value: object, label: str) -> float:
    if type(value) not in (int, float) or not math.isfinite(value):
        raise ValueError(f"{label} is not a finite JSON number")
    return float(value)


def _validate_reference_case(case_id: str, value: object) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {
        "rendered_prompt_sha256",
        "prompt_token_ids",
        "initial_top16_log_probabilities",
        "initial_top1_margin",
        "greedy_token_ids",
        "greedy_step_top1_margins",
    }:
        raise ValueError(f"oracle case {case_id!r} has an invalid shape")
    tokens = value["prompt_token_ids"]
    greedy = value["greedy_token_ids"]
    margins = value["greedy_step_top1_margins"]
    top16 = value["initial_top16_log_probabilities"]
    if (
        not isinstance(tokens, list)
        or not tokens
        or any(type(token) is not int or token < 0 for token in tokens)
        or not isinstance(greedy, list)
        or len(greedy) != 8
        or any(type(token) is not int or token < 0 for token in greedy)
        or not isinstance(margins, list)
        or len(margins) != 8
        or not isinstance(top16, list)
        or len(top16) != 16
    ):
        raise ValueError(f"oracle case {case_id!r} has invalid bounded arrays")
    _finite_number(value["initial_top1_margin"], f"{case_id}.initial margin")
    for index, margin in enumerate(margins):
        _finite_number(margin, f"{case_id}.greedy margin {index}")
    seen: set[int] = set()
    previous: tuple[float, int] | None = None
    for index, entry in enumerate(top16):
        if not isinstance(entry, dict) or set(entry) != {"token_id", "log_probability"}:
            raise ValueError(f"oracle case {case_id!r} top16 entry is malformed")
        token_id = entry["token_id"]
        probability = _finite_number(
            entry["log_probability"], f"{case_id}.top16[{index}]"
        )
        if type(token_id) is not int or token_id < 0 or token_id in seen:
            raise ValueError(f"oracle case {case_id!r} top16 token is invalid")
        seen.add(token_id)
        order = (-probability, token_id)
        if previous is not None and order < previous:
            raise ValueError(f"oracle case {case_id!r} top16 order is invalid")
        previous = order
    return value


def _float_logits(model: Any, tokens: list[int]) -> list[float]:
    logits = model.full_prefix_logits(tokens)
    host_logits = logits.float().to("CPU")
    values = host_logits.tolist()
    if not isinstance(values, list) or len(values) != 1 or not isinstance(values[0], list):
        raise ValueError("candidate logits have an invalid rank")
    values = values[0]
    if len(values) != model.contract.config["vocab_size"]:
        raise ValueError("candidate logits have the wrong vocabulary size")
    output = [_finite_number(value, "candidate logit") for value in values]
    # Each oracle step is an independent full-prefix observation. Release the
    # realized graph before the next step, including tinygrad's LRU of transient
    # buffers; admitted model weights remain live through model._state.
    del host_logits, logits
    gc.collect()
    from tinygrad import Device

    Device[Device.DEFAULT].synchronize()
    Device[Device.DEFAULT].allocator.free_cache()
    Device["CPU"].allocator.free_cache()
    return output


def _projection(logits: list[float]) -> tuple[list[dict[str, object]], int, float]:
    ranked = sorted(range(len(logits)), key=lambda token: (-logits[token], token))
    top = ranked[:16]
    maximum = logits[top[0]]
    log_normalizer = maximum + math.log(
        math.fsum(math.exp(value - maximum) for value in logits)
    )
    projection = [
        {"token_id": token, "log_probability": logits[token] - log_normalizer}
        for token in top
    ]
    return projection, top[0], logits[top[0]] - logits[top[1]]


def _observe_case(model: Any, prompt_tokens: list[int]) -> dict[str, object]:
    initial, _initial_token, initial_margin = _projection(
        _float_logits(model, prompt_tokens)
    )
    prefix = list(prompt_tokens)
    greedy_tokens: list[int] = []
    greedy_margins: list[float] = []
    for _ in range(8):
        _top16, token, margin = _projection(_float_logits(model, prefix))
        greedy_tokens.append(token)
        greedy_margins.append(margin)
        prefix.append(token)
    return {
        "initial_top16_log_probabilities": initial,
        "initial_top1_margin": initial_margin,
        "greedy_token_ids": greedy_tokens,
        "greedy_step_top1_margins": greedy_margins,
    }


def _compare_case(
    reference: dict[str, Any], candidate: dict[str, Any], thresholds: dict[str, Any]
) -> dict[str, object]:
    reference_by_token = {
        entry["token_id"]: float(entry["log_probability"])
        for entry in reference["initial_top16_log_probabilities"]
    }
    candidate_by_token = {
        entry["token_id"]: float(entry["log_probability"])
        for entry in candidate["initial_top16_log_probabilities"]
    }
    matched = sorted(set(reference_by_token) & set(candidate_by_token))
    errors = [abs(reference_by_token[token] - candidate_by_token[token]) for token in matched]
    overlap = len(matched)
    maximum_error = max(errors) if errors else None
    mean_error = math.fsum(errors) / overlap if errors else None
    checks = {
        "top1_token_matches": (
            candidate["initial_top16_log_probabilities"][0]["token_id"]
            == reference["initial_top16_log_probabilities"][0]["token_id"]
        ),
        "top16_overlap_sufficient": overlap >= thresholds["minimum_top16_token_overlap"],
        "maximum_error_within_bound": maximum_error is not None
        and maximum_error
        <= thresholds["maximum_matched_log_probability_absolute_error"],
        "mean_error_within_bound": mean_error is not None
        and mean_error
        <= thresholds["maximum_matched_log_probability_mean_absolute_error"],
        "greedy_tokens_match": (
            candidate["greedy_token_ids"] == reference["greedy_token_ids"]
        ),
    }
    return {
        "passed": all(checks.values()),
        "checks": checks,
        "top16_overlap": overlap,
        "matched_token_ids": matched,
        "maximum_matched_log_probability_absolute_error": maximum_error,
        "matched_log_probability_mean_absolute_error": mean_error,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--request", required=True, type=Path)
    parser.add_argument("--oracle", required=True, type=Path)
    parser.add_argument("--model-root", required=True, type=Path)
    parser.add_argument("--model-manifest-digest", required=True)
    parser.add_argument("--candidate-environment", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    request_bytes, request = _load_json(args.request)
    _oracle_bytes, oracle = _load_json(args.oracle)
    _environment_bytes, environment = _load_json(args.candidate_environment)
    request_sha256 = hashlib.sha256(request_bytes).hexdigest()
    if oracle.get("schema") != EXPECTED_ORACLE_SCHEMA:
        raise ValueError("numeric oracle schema is not exact")
    if oracle.get("request_sha256") != request_sha256:
        raise ValueError("numeric oracle is bound to a different request")
    if oracle.get("model_manifest_digest") != args.model_manifest_digest:
        raise ValueError("numeric oracle is bound to a different model tree")
    if request.get("scope", {}).get("consequences") != "none":
        raise ValueError("numeric request has an activation consequence")
    cases = request.get("cases")
    oracle_cases = oracle.get("cases")
    thresholds = request.get("comparison_thresholds")
    if not isinstance(cases, list) or not isinstance(oracle_cases, dict):
        raise ValueError("numeric request or oracle cases are malformed")
    if not isinstance(thresholds, dict):
        raise ValueError("numeric comparison thresholds are malformed")
    expected_ids = [case.get("id") for case in cases if isinstance(case, dict)]
    if len(expected_ids) != len(cases) or set(expected_ids) != set(oracle_cases):
        raise ValueError("numeric oracle case set differs from the request")
    minimum_margin = _finite_number(
        thresholds.get("minimum_reference_top1_margin"), "minimum reference margin"
    )
    validated_oracle = {
        case_id: _validate_reference_case(case_id, oracle_cases[case_id])
        for case_id in expected_ids
    }
    if any(
        float(case["initial_top1_margin"]) < minimum_margin
        or any(float(margin) < minimum_margin for margin in case["greedy_step_top1_margins"])
        for case in validated_oracle.values()
    ):
        raise ValueError("numeric oracle has an insufficient reference margin")

    from model import QwenModel
    from tokenizer import QwenTokenizer, render_chat

    started = time.monotonic()
    tokenizer = QwenTokenizer(args.model_root, request["model"]["model_id"])
    model = QwenModel(args.model_root, request["model"]["model_id"])
    loaded = time.monotonic()
    candidate_cases: dict[str, object] = {}
    comparisons: dict[str, object] = {}
    for case in cases:
        case_id = case["id"]
        rendered = render_chat(
            case["messages"], case["tools"], enable_thinking=case["enable_thinking"]
        )
        encoded = rendered.encode("utf-8")
        if rendered != case["rendered_prompt_utf8"] or hashlib.sha256(encoded).hexdigest() != case["rendered_prompt_sha256"]:
            raise ValueError(f"candidate renderer differs for case {case_id!r}")
        prompt_tokens = tokenizer.encode(rendered)
        reference = validated_oracle[case_id]
        if prompt_tokens != reference["prompt_token_ids"]:
            raise ValueError(f"candidate tokenizer differs for case {case_id!r}")
        observed = _observe_case(model, prompt_tokens)
        observed["rendered_prompt_sha256"] = case["rendered_prompt_sha256"]
        observed["prompt_token_ids"] = prompt_tokens
        candidate_cases[case_id] = observed
        comparisons[case_id] = _compare_case(reference, observed, thresholds)
    completed = time.monotonic()
    passed = all(value["passed"] for value in comparisons.values())
    result = {
        "schema": RESULT_SCHEMA,
        "scope": "numeric_conformance_only",
        "consequences": "none",
        "passed": passed,
        "request_sha256": request_sha256,
        "model_manifest_digest": args.model_manifest_digest,
        "reference_environment_manifest_digest": oracle[
            "reference_environment_manifest_digest"
        ],
        "candidate_environment": environment,
        "cases": candidate_cases,
        "comparisons": comparisons,
        "timing_seconds": {
            "model_load": loaded - started,
            "candidate_observation": completed - loaded,
            "total": completed - started,
        },
    }
    args.output.write_text(
        json.dumps(result, ensure_ascii=False, sort_keys=True, separators=(",", ":")),
        encoding="utf-8",
    )
    if not passed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
