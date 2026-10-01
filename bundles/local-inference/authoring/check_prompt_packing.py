#!/usr/bin/env python3
"""Check fully rendered prompts with exact local Qwen tokenizer metadata.

This source-preparation utility performs no download, model import, inference,
activation or qualification. Supply complete frozen request messages; byte
lengths and synthetic test tokenizers never count as measured token fit.
"""
from __future__ import annotations

import argparse
import ast
import hashlib
import importlib
import json
import os
from pathlib import Path
import platform
import stat
import sys
import unicodedata
from typing import Callable


BUNDLE = Path(__file__).resolve().parent.parent
REPOSITORY = BUNDLE.parent.parent
WORKER_SOURCE = BUNDLE / ".ai/workers/local-inference/lib/local-tinygrad"
MAX_REQUEST_BYTES = 512 * 1024
MAX_REQUESTS = 64
METADATA_FILES = {"config.json", "generation_config.json", "tokenizer.json", "tokenizer_config.json"}


def strict_object(pairs: list[tuple[str, object]]) -> dict:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def reject_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON constant: {value}")


def expected_metadata(profile: str) -> dict[str, str]:
    if profile == "qwen3-0.6b":
        source = REPOSITORY / "scripts/release/author-local-inference-realizations.py"
        parsed = ast.parse(source.read_text())
        files = next(ast.literal_eval(node.value) for node in parsed.body
                     if isinstance(node, ast.Assign) and any(
                         isinstance(target, ast.Name) and target.id == "MODEL_FILES"
                         for target in node.targets))
        return {name: files[name] for name in sorted(METADATA_FILES)}
    if profile == "qwen3-4b":
        source = BUNDLE / "authoring/contracts/qwen3-4b-bf16-model-source-v1.json"
        contract = json.loads(source.read_text(), object_pairs_hook=strict_object,
                              parse_constant=reject_constant)
        return {entry["path"]: entry["sha256"] for entry in contract["files"]
                if entry["path"] in METADATA_FILES}
    raise ValueError("no inspected metadata contract for this profile")


def verify_metadata(root: Path, expected: dict[str, str]) -> dict[str, dict]:
    if set(expected) != METADATA_FILES:
        raise ValueError("tokenizer metadata identity set is incomplete")
    observed = {}
    for name, wanted in sorted(expected.items()):
        path = root / name
        descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, "rb") as source:
            before = os.fstat(source.fileno())
            maximum = 16 * 1024 * 1024 if name == "tokenizer.json" else 64 * 1024
            if not stat.S_ISREG(before.st_mode) or not 0 < before.st_size <= maximum:
                raise ValueError(f"metadata file exceeds its regular-file bound: {name}")
            digest = hashlib.file_digest(source, "sha256").hexdigest()
            after = os.fstat(source.fileno())
            fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
            if any(getattr(before, key) != getattr(after, key) for key in fields) or digest != wanted:
                raise ValueError(f"metadata identity changed: {name}")
        observed[name] = {"bytes": before.st_size, "sha256": digest}
    return observed


def worker_tokenizer_module():
    # This uses the existing pure tokenizer/model-contract source, never the
    # session bootstrap or tinygrad/model module (which can initialize devices).
    sys.path.insert(0, str(WORKER_SOURCE))
    try:
        module = importlib.import_module("tokenizer")
        contract = importlib.import_module("model_contract")
    finally:
        sys.path.pop(0)
    for name, imported in [("tokenizer.py", module), ("model_contract.py", contract)]:
        if Path(imported.__file__).resolve() != (WORKER_SOURCE / name).resolve():
            raise ValueError("preimported tokenizer code is outside the inspected source")
    return module


def pack_requests(requests: object, *, encode: Callable, render: Callable,
                  input_limit: int, output_limit: int, context_limit: int,
                  profile_output_limit: int) -> list[dict]:
    for value in (input_limit, output_limit, context_limit, profile_output_limit):
        if type(value) is not int or value <= 0:
            raise ValueError("token ceilings must be positive integers")
    if input_limit > context_limit or output_limit > profile_output_limit:
        raise ValueError("packing budget exceeds the selected profile")
    if not isinstance(requests, list) or not 1 <= len(requests) <= MAX_REQUESTS:
        raise ValueError("frozen request set exceeds its bound")
    ids = set()
    rows = []
    for request in requests:
        if not isinstance(request, dict) or set(request) != {"id", "messages", "tools", "enable_thinking"}:
            raise ValueError("each frozen request needs id/messages/tools/enable_thinking")
        name = request["id"]
        if not isinstance(name, str) or not 0 < len(name) <= 128 or name in ids:
            raise ValueError("request identities must be bounded and unique")
        ids.add(name)
        if (not isinstance(request["messages"], list) or not request["messages"]
                or len(request["messages"]) > 512 or not isinstance(request["tools"], list)
                or len(request["tools"]) > 256 or type(request["enable_thinking"]) is not bool):
            raise ValueError("request messages/tools/thinking shape is invalid")
        prompt = render(request["messages"], request["tools"], request["enable_thinking"])
        tokens = encode(prompt)
        if not isinstance(tokens, list) or any(type(token) is not int or token < 0 for token in tokens):
            raise ValueError("tokenizer returned invalid token identities")
        count = len(tokens)
        refusal = ("rendered_input_exceeds_limit" if count > input_limit else
                   "input_plus_output_exceeds_context" if count + output_limit > context_limit else None)
        prompt_bytes = prompt.encode("utf-8")
        token_bytes = json.dumps(tokens, separators=(",", ":")).encode()
        rows.append({"id": name, "rendered_input_tokens": count,
                     "reserved_output_tokens": output_limit,
                     "combined_tokens": count + output_limit,
                     "rendered_bytes": len(prompt_bytes),
                     "rendered_sha256": hashlib.sha256(prompt_bytes).hexdigest(),
                     "token_ids_sha256": hashlib.sha256(token_bytes).hexdigest(),
                     "fits": refusal is None, "refusal": refusal})
    return rows


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata-root", type=Path, required=True)
    parser.add_argument("--requests", type=Path, required=True,
                        help="JSON array of fully frozen id/messages/tools/enable_thinking requests")
    parser.add_argument("--profile", choices=["qwen3-0.6b", "qwen3-4b"], required=True)
    parser.add_argument("--input-limit", type=int, required=True)
    parser.add_argument("--output-limit", type=int, required=True)
    args = parser.parse_args()
    with args.requests.open("rb") as source:
        encoded = source.read(MAX_REQUEST_BYTES + 1)
    if not 0 < len(encoded) <= MAX_REQUEST_BYTES:
        raise ValueError("packing request file exceeds its byte bound")
    requests = json.loads(encoded, object_pairs_hook=strict_object, parse_constant=reject_constant)
    expected = expected_metadata(args.profile)
    observed = verify_metadata(args.metadata_root, expected)
    module = worker_tokenizer_module()
    profile = module.load_named_model_profile(args.profile)
    tokenizer = module.QwenTokenizer(args.metadata_root, args.profile)
    rows = pack_requests(requests, encode=tokenizer.encode, render=module.render_chat,
                         input_limit=args.input_limit, output_limit=args.output_limit,
                         context_limit=profile.context_ceiling, profile_output_limit=profile.output_ceiling)
    if observed != verify_metadata(args.metadata_root, expected):
        raise ValueError("tokenizer metadata changed during packing")
    print(json.dumps({"schema": "ryeos.local_inference.prompt_packing.v1",
                      "evidence_class": "local_exact_tokenizer_packing_only_no_model_or_device",
                      "profile": args.profile, "requests_sha256": hashlib.sha256(encoded).hexdigest(),
                      "metadata": observed, "python": platform.python_version(),
                      "unicode_database": unicodedata.unidata_version,
                      "tokenizer_source_sha256": hashlib.sha256((WORKER_SOURCE / "tokenizer.py").read_bytes()).hexdigest(),
                      "input_limit": args.input_limit, "output_limit": args.output_limit,
                      "context_limit": profile.context_ceiling, "rows": rows,
                      "all_fit": all(row["fits"] for row in rows)}, indent=2))
    return 0 if all(row["fits"] for row in rows) else 1


def cli() -> int:
    try:
        return main()
    except (OSError, ValueError) as error:
        print(json.dumps({"schema": "ryeos.local_inference.prompt_packing_refusal.v1",
                          "status": "refused_no_packing_evidence",
                          "reason": str(error), "actual_token_counts_available": False,
                          "model_or_device_contacts": 0}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(cli())
