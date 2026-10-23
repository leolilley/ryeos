"""Build the canonical finite product contract literal for the verifier Tool.

The literal is the third argument of consumer-runtime-verifier.yaml. It is
canonical JSON (sorted keys, compact separators) exactly as the verifier's
strict parser accepts it. Fixed policy strings live here as data; the helper
manifest digests are read from the external Worker's `external_content`, the
checker runtime manifest hash from the Tool's `checker-runtime` literal, and
the checker script size and digest from the shipped file at generation time.
The four values captured on a live node come from a JSON file:
`code_manifest_hash`, `profile_hash`, `source_binding_hash`,
`source_content_manifest_hash`.

Every missing value, every non-64-hex hash and any literal over 8192 bytes is
refused. `--placeholders` emits refusable `@@NAME@@` tokens in place of the
captured values (the verifier rejects such a literal); it exists only so the
checked-in Tool is regenerable before the live capture. `--write-tool`
rewrites the Tool's third argument in place and keeps its signed header line
untouched; the publisher re-signs afterwards. Requires PyYAML.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sys

import yaml

BUNDLE = Path(__file__).resolve().parents[6]
TOOL = BUNDLE / ".ai/tools/codex/qualification/consumer-runtime-verifier.yaml"
WORKER = BUNDLE / ".ai/workers/codex/external-hosted-authoring.yaml"
CHECKER = BUNDLE / ".ai/tools/codex/qualification/lib/semantic-checker/checker.py"

MAX_LITERAL_BYTES = 8192
HEX64 = re.compile(r"[0-9a-f]{64}")
SIGNED_HEADER = "# ryeos:signed:"
CAPTURED = (
    "code_manifest_hash",
    "profile_hash",
    "source_binding_hash",
    "source_content_manifest_hash",
)
PLACEHOLDERS = {name: "@@%s@@" % name.upper() for name in CAPTURED}
HELPER_IDS = ("codex", "codex-bwrap", "codex-code-mode-host", "codex-rg", "codex-zsh")
RUNTIME_ID = "checker-runtime"
CODE_ID = "checker-code"
ARG_FLAG = "--product-contract"

# Members of the pinned runtime tree, measured from the tree itself.
RUNTIME_MEMBERS = {
    "loader": {
        "path": "lib/ld-musl-x86_64.so.1",
        "bytes": 662120,
        "sha256": "1cc5c3f502179e2a4befae29658fb109102effefc75742893aa4a449fd3fbb03",
    },
    "interpreter": {
        "path": "python/bin/python3.14",
        "bytes": 22166488,
        "sha256": "2d8886b2bad1105014ce23e2439b20d47a78416281b906e21ac0e37085d19e9d",
    },
}

FIXED = {
    "schema": "ryeos.codex.finite_product_contract.v2",
    "audit_edition": "codex-0.147.0-interpreted-remote-environment-v3",
    "families": {
        "code_mode": "bounded-compute-no-imports-same-nested-router",
        "deferred_tools": "presentation-only-no-added-authority",
        "delegation": "original-checked-agents-disabled",
        "exec_and_stdin": "selected-remote-connector-no-local-fallback",
        "file_and_image_reads": "selected-environment-filesystem",
        "guardian": "guardian-context-only-and-request-permissions-empty-under-never-approval",
        "mcp_apps_plugins_skills": "original-checked-disabled-or-empty",
        "memories_and_goals": "original-checked-disabled",
        "session_utilities": "bounded-session-bookkeeping-and-existing-environment-wait-only",
        "shell_patch_interception": "selected-environment-filesystem",
        "web_and_image_generation": "original-checked-disabled",
    },
    "inputs": {
        "cwd": "/workspace",
        "environment": "sole-default-command-connector",
        "include_local": False,
        "ordinary_request_language": "closed-start-resume-and-owner-bound-plain-text-turns-v1",
        "request_language": "closed-session-start-and-input-only-canary-turn",
        "session_setup": "raw-events-true-and-optional-canonical-model-only",
    },
    "permitted_controller_services": [
        "auth-model-config",
        "git-attribution-provider-metadata",
        "rollout-history",
        "telemetry-cache",
        "bounded-compute-timers-session-store",
    ],
    "setup": {
        "approval_policy": "never",
        "approvals_reviewer": "user",
        "model_provider": "openai",
        "model_selection": "compare-only-explicit-signed-selection",
        "permission_profile": "danger-full-access",
        "sandbox": "dangerFullAccess",
    },
    "worker_ref": "worker:codex/external-hosted-authoring",
}


class Refusal(Exception):
    pass


def strip_header(text: str) -> str:
    lines = text.splitlines(keepends=True)
    if lines and lines[0].startswith(SIGNED_HEADER):
        lines = lines[1:]
    return "".join(lines)


def load_yaml(path: Path) -> dict:
    return yaml.safe_load(strip_header(path.read_text(encoding="utf-8")))


def require_hex(label: str, value: object) -> str:
    if not isinstance(value, str) or not HEX64.fullmatch(value):
        raise Refusal("%s must be 64 lowercase hex characters" % label)
    return value


def load_captured(path: Path) -> dict:
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise Refusal("captured values must be a JSON object")
    missing = [name for name in CAPTURED if name not in data]
    if missing:
        raise Refusal("captured values missing: %s" % ", ".join(missing))
    extra = sorted(set(data) - set(CAPTURED))
    if extra:
        raise Refusal("unexpected captured values: %s" % ", ".join(extra))
    return {name: require_hex(name, data[name]) for name in CAPTURED}


def helper_digests(worker: Path) -> dict:
    entries = load_yaml(worker)["external_content"]
    helpers = {entry["id"]: require_hex("helper " + entry["id"], entry["digest"]) for entry in entries}
    if sorted(helpers) != list(HELPER_IDS):
        raise Refusal("worker external_content is not the exact five helper roles")
    return helpers


def runtime_manifest_hash(tool: Path) -> str:
    matches = [e for e in load_yaml(tool).get("external_content", []) if e.get("id") == RUNTIME_ID]
    if len(matches) != 1:
        raise Refusal("tool must declare exactly one %s external_content literal" % RUNTIME_ID)
    return require_hex("runtime digest", matches[0]["digest"])


def build_contract(captured: dict, tool: Path = TOOL, worker: Path = WORKER, checker: Path = CHECKER) -> dict:
    code = checker.read_bytes()
    if not code:
        raise Refusal("checker script is empty")
    return {
        "schema": FIXED["schema"],
        "audit_edition": FIXED["audit_edition"],
        "target": {
            "worker_ref": FIXED["worker_ref"],
            "profile_hash": captured["profile_hash"],
            "source_binding_hash": captured["source_binding_hash"],
            "source_content_manifest_hash": captured["source_content_manifest_hash"],
        },
        "helpers": helper_digests(worker),
        "inputs": FIXED["inputs"],
        "setup": FIXED["setup"],
        "families": FIXED["families"],
        "permitted_controller_services": FIXED["permitted_controller_services"],
        "semantic_checker": {
            "schema": "ryeos.codex.semantic_checker_program.v1",
            "wire_schema": "ryeos.codex.semantic_input.v2",
            "runtime": {
                "id": RUNTIME_ID,
                "manifest_hash": runtime_manifest_hash(tool),
                "loader": RUNTIME_MEMBERS["loader"],
                "interpreter": RUNTIME_MEMBERS["interpreter"],
                "library_path": "lib",
                "prefix_path": "python",
            },
            "code": {
                "id": CODE_ID,
                "manifest_hash": captured["code_manifest_hash"],
                "script": {
                    "path": "checker.py",
                    "bytes": len(code),
                    "sha256": hashlib.sha256(code).hexdigest(),
                },
            },
            "max_header_bytes": 2 * 1024 * 1024,
            "max_response_bytes": 64 * 1024,
            "required_root_settlement": "scope_empty",
        },
    }


def canonical(contract: dict) -> str:
    literal = json.dumps(contract, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    if len(literal.encode("utf-8")) > MAX_LITERAL_BYTES:
        raise Refusal("literal exceeds %d bytes" % MAX_LITERAL_BYTES)
    return literal


def generate(captured: dict | None, placeholders: bool, **paths: Path) -> str:
    if placeholders:
        if captured is not None:
            raise Refusal("--placeholders takes no captured values")
        captured = dict(PLACEHOLDERS)
    elif captured is None:
        raise Refusal("captured values are required")
    return canonical(build_contract(captured, **paths))


def write_tool(tool: Path, literal: str) -> None:
    if "'" in literal or "\n" in literal:
        raise Refusal("literal is not safe as a single-quoted YAML scalar")
    lines = tool.read_text(encoding="utf-8").splitlines(keepends=True)
    flags = [i for i, line in enumerate(lines) if line.strip() == "- " + ARG_FLAG]
    if len(flags) != 1 or flags[0] + 1 >= len(lines):
        raise Refusal("tool must carry exactly one %s argument" % ARG_FLAG)
    target = flags[0] + 1
    indent = lines[flags[0]][: len(lines[flags[0]]) - len(lines[flags[0]].lstrip())]
    lines[target] = "%s- '%s'\n" % (indent, literal)
    updated = "".join(lines)
    arg = load_yaml_text(updated)["config"]["args"]
    if arg[arg.index(ARG_FLAG) + 1] != literal:
        raise Refusal("rewritten tool does not round-trip the literal")
    staged = tool.with_name(tool.name + ".tmp")
    staged.write_text(updated, encoding="utf-8")
    os.replace(staged, tool)


def load_yaml_text(text: str) -> dict:
    return yaml.safe_load(strip_header(text))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--captured", type=Path, help="JSON file of live-captured values")
    parser.add_argument("--placeholders", action="store_true", help="emit refusable @@NAME@@ tokens")
    parser.add_argument("--write-tool", action="store_true", help="rewrite the Tool's third argument in place")
    parser.add_argument("--tool", type=Path, default=TOOL)
    parser.add_argument("--worker", type=Path, default=WORKER)
    parser.add_argument("--checker", type=Path, default=CHECKER)
    args = parser.parse_args(argv)
    try:
        captured = load_captured(args.captured) if args.captured else None
        literal = generate(captured, args.placeholders, tool=args.tool, worker=args.worker, checker=args.checker)
        if args.write_tool:
            write_tool(args.tool, literal)
        else:
            sys.stdout.write(literal + "\n")
    except Refusal as error:
        sys.stderr.write("refused: %s\n" % error)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
