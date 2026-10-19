"""Merge a derived consumer qualification context into a consumer policy.

Reads the JSON emitted by the daemon's read-only consumer qualification
context derivation and a policy file whose Codex-side data is already authored
(producer scenarios, command expectations, responses origin, controller,
qualification worker, outer network). Writes the policy with the derived
context merged into `verifier_parameters` and `consumer_execution_context`.

This tool invents nothing: every missing or malformed field is refused. It
neither signs nor verifies signatures; the written file carries no signed
header and must be signed by the publisher afterwards. Requires PyYAML.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys

import yaml

CONTEXT_SCHEMA = "ryeos.external_candidate_qualification_context.v3"
DIGEST_FIELDS = (
    "requirement_digest",
    "profile_hash",
    "source_binding_hash",
    "source_content_manifest_hash",
    "provider_executable_manifest_hash",
    "execution_environment_digest",
)
CONSUMER_CONTEXT_FIELDS = (
    "worker_ref",
    "product_declaration_id",
    "environment_ref",
    "worker_execution_ref",
    "environment_binding",
)
EMITTED_FIELDS = (
    "policy_ref",
    "relationship_name",
    "subject_manifest_hash",
    "bundle_generation_identity",
    "verifier_parameters",
    "consumer_execution_context",
)
CODEX_SIDE_FIELDS = (
    "controller",
    "expected_command_output",
    "responses_origin",
    "outer_network",
    "qualification_worker",
)
VERIFIER_SCHEMA = "ryeos.codex.consumer_verifier.v4"
VERIFIER_TOOL = (
    Path(__file__).resolve().parents[2] / "consumer-runtime-verifier.yaml"
)
HEX64 = re.compile(r"^[0-9a-f]{64}$")
SIGNED_HEADER = "# ryeos:signed:"


class MergeRefused(Exception):
    pass


def refuse(message: str):
    raise MergeRefused(message)


def exact_keys(value, expected, what: str) -> dict:
    if not isinstance(value, dict):
        refuse(what + " must be an object")
    missing = [k for k in expected if k not in value]
    extra = [k for k in value if k not in expected]
    if missing or extra:
        refuse(what + ": missing " + repr(missing) + ", unexpected " + repr(extra))
    return value


def text(value, what: str) -> str:
    if not isinstance(value, str) or not value:
        refuse(what + " must be a non-empty string")
    return value


def hex64(value, what: str) -> str:
    if not isinstance(value, str) or not HEX64.match(value):
        refuse(what + " must be 64 lowercase hex characters")
    return value


def parse_emitted(document) -> dict:
    exact_keys(document, EMITTED_FIELDS, "emitted document")
    text(document["policy_ref"], "policy_ref")
    text(document["relationship_name"], "relationship_name")
    hex64(document["subject_manifest_hash"], "subject_manifest_hash")
    text(document["bundle_generation_identity"], "bundle_generation_identity")
    parameters = exact_keys(
        document["verifier_parameters"], ("external_candidate_qualification_context",),
        "emitted verifier_parameters",
    )
    context = exact_keys(
        parameters["external_candidate_qualification_context"],
        ("schema",) + DIGEST_FIELDS,
        "external_candidate_qualification_context",
    )
    if context["schema"] != CONTEXT_SCHEMA:
        refuse("external_candidate_qualification_context.schema must be " + CONTEXT_SCHEMA)
    for field in DIGEST_FIELDS:
        hex64(context[field], "external_candidate_qualification_context." + field)
    consumer = exact_keys(
        document["consumer_execution_context"], CONSUMER_CONTEXT_FIELDS,
        "consumer_execution_context",
    )
    for field in CONSUMER_CONTEXT_FIELDS:
        text(consumer[field], "consumer_execution_context." + field)
    return document


def validate_schema(value, schema: dict, where: str) -> None:
    """Structural check for the subset of JSON Schema the verifier uses."""
    expected = schema.get("type")
    if expected is not None:
        names = expected if isinstance(expected, list) else [expected]
        kinds = {
            "object": lambda v: isinstance(v, dict),
            "string": lambda v: isinstance(v, str),
            "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
            "null": lambda v: v is None,
        }
        if not any(kinds[name](value) for name in names):
            refuse(where + " has the wrong type")
    if "enum" in schema and value not in schema["enum"]:
        refuse(where + " is not an allowed value")
    if isinstance(value, str):
        if len(value) < schema.get("minLength", 0):
            refuse(where + " is too short")
        if len(value) > schema.get("maxLength", len(value)):
            refuse(where + " is too long")
        if "pattern" in schema and not re.search(schema["pattern"], value):
            refuse(where + " does not match its pattern")
    if isinstance(value, int) and not isinstance(value, bool):
        if value < schema.get("minimum", value) or value > schema.get("maximum", value):
            refuse(where + " is out of range")
    if isinstance(value, dict) and "properties" in schema:
        for name in schema.get("required", []):
            if name not in value:
                refuse(where + " is missing " + name)
        for name, item in value.items():
            if name in schema["properties"]:
                validate_schema(item, schema["properties"][name], where + "." + name)
            elif schema.get("additionalProperties") is False:
                refuse(where + " has unexpected field " + name)


def verifier_config_schema(path: Path = VERIFIER_TOOL) -> dict:
    tool = yaml.safe_load(strip_header(path.read_text(encoding="utf-8")))
    schema = tool["config_schema"]
    if schema["properties"]["schema"]["enum"] != [VERIFIER_SCHEMA]:
        refuse("verifier tool no longer declares " + VERIFIER_SCHEMA)
    return schema


def strip_header(source: str) -> str:
    lines = source.split("\n")
    if lines and lines[0].startswith(SIGNED_HEADER):
        return "\n".join(lines[1:])
    return source


def merge(policy: dict, emitted: dict, replace: bool, schema: dict) -> bool:
    """Merge in place; returns whether the policy changed."""
    section = policy.get("product_qualification_policy")
    if not isinstance(section, dict):
        refuse("policy has no product_qualification_policy object")
    parameters = section.get("verifier_parameters")
    if not isinstance(parameters, dict):
        refuse("policy verifier_parameters must be an object carrying the Codex-side data")
    for field in CODEX_SIDE_FIELDS:
        if field not in parameters:
            refuse("policy verifier_parameters is missing Codex-side " + field)
    if parameters.get("schema") != VERIFIER_SCHEMA:
        refuse("policy verifier_parameters.schema must be " + VERIFIER_SCHEMA)
    scenarios = section.get("producer_scenarios")
    if not isinstance(scenarios, dict) or not scenarios:
        refuse("policy has no producer_scenarios")
    scenario_id = parameters["controller"].get("scenario_id") if isinstance(
        parameters["controller"], dict) else None
    if scenario_id not in scenarios:
        refuse("controller.scenario_id names no producer scenario")

    derived = emitted["verifier_parameters"]["external_candidate_qualification_context"]
    consumer = emitted["consumer_execution_context"]
    changed = False
    for key, container, value in (
        ("external_candidate_qualification_context", parameters, derived),
        ("consumer_execution_context", section, consumer),
    ):
        if key in container and container[key] != value and not replace:
            refuse(key + " already differs from the emitted value; pass --replace")
        if container.get(key) != value:
            container[key] = value
            changed = True
    validate_schema(parameters, schema, "verifier_parameters")
    return changed


def run(emitted_text: str, policy_path: Path, output: Path, replace: bool) -> bool:
    try:
        emitted = parse_emitted(json.loads(emitted_text))
    except json.JSONDecodeError as error:
        refuse("emitted document is not JSON: " + str(error))
    policy = yaml.safe_load(strip_header(policy_path.read_text(encoding="utf-8")))
    if not isinstance(policy, dict):
        refuse("policy is not a mapping")
    changed = merge(policy, emitted, replace, verifier_config_schema())
    if changed or output != policy_path:
        output.write_text(
            yaml.safe_dump(policy, sort_keys=False, default_flow_style=False),
            encoding="utf-8",
        )
    return changed


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("policy", type=Path, help="consumer policy template YAML")
    parser.add_argument("--emitted", type=Path, help="emitter JSON file (default: stdin)")
    parser.add_argument("--output", type=Path, help="write here (default: the policy path)")
    parser.add_argument("--replace", action="store_true",
                        help="replace an already populated, different qualification context")
    args = parser.parse_args(argv)
    source = args.emitted.read_text(encoding="utf-8") if args.emitted else sys.stdin.read()
    output = args.output or args.policy
    try:
        changed = run(source, args.policy, output, args.replace)
    except MergeRefused as error:
        print("refused: " + str(error), file=sys.stderr)
        return 1
    print(("merged into " if changed else "already populated, unchanged: ") + str(output))
    print("sign with, from the repository root: ./scripts/dev/sign-dev.sh " + str(output))
    return 0


if __name__ == "__main__":
    sys.exit(main())
