# ryeos:signed:2026-09-17T02:47:27Z:17205589e7b2720e8e36cce550b051c723e63fba3b9d8b4471b5474261a7bc81:G6LNdC3w/5r2VwhjnhtXDmfcXKV/sJeGRmyFlm0LGfQM8jDZH9Ym5CHurSAuNwo6GfDgRwvSi6mvgddDo6cBAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/hosted-oci-runtime
#   version: "1.0.0"
#   description: Validate bounded installed hard-contained OCI qualification evidence
#   executor_id: tool:ryeos/development/authoring-environment-production/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   workspace_access: immutable_current_generation
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties:
#       evidence:
#         type: object
#     required: [evidence]
#     additionalProperties: false
#   config_resolve:
#     type: single
#     spec:
#       path: development/ryeos/hosted-oci-runtime.yaml
#       mode: first_match
"""Validate evidence shape and completeness; never provision or contact a host."""

import argparse
import json
import re
import sys


DIGEST = re.compile(r"sha256:[a-f0-9]{64}")
REQUIRED_FIELDS = {
    "schema", "claim_class", "source_revision", "image_digest", "profile_digest", "policy_digest",
    "node_fingerprint", "binding_digest", "controller_account", "lifecycle_identity",
    "hook_digest", "lifetime_generation", "observations", "capabilities", "refusals",
    "attestation",
}


def string_set(value, name):
    if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
        raise ValueError(f"{name} must be a string list")
    if len(value) != len(set(value)):
        raise ValueError(f"{name} contains duplicates")
    return set(value)


def evaluate(request):
    if not isinstance(request, dict) or set(request) != {"resolved_config", "evidence"}:
        raise ValueError("qualification request is not closed")
    config = request["resolved_config"]
    if not isinstance(config, dict) or config.get("schema") != "ryeos.development.hosted-oci-runtime.v1":
        raise ValueError("wrong hosted OCI qualification config")
    if set(config) != {
        "category", "name", "version", "schema", "description",
        "runtime_product", "installed_attestor", "topology", "lifecycle",
        "required_capabilities", "required_refusals", "required_observations",
        "claim_classes", "limits",
    }:
        raise ValueError("hosted OCI qualification config is not closed")
    if config.get("runtime_product") != {
        "image_target": "ryeos-contained-workflow",
        "node_profile": "contained-workflow",
        "controller_account": {"implementation": "unix", "uid": 10001, "gid": 10001},
    }:
        raise ValueError("wrong hosted OCI runtime product")
    evidence = request["evidence"]
    if not isinstance(evidence, dict) or set(evidence) != REQUIRED_FIELDS:
        raise ValueError("installed evidence is not a closed current record")
    if evidence["schema"] != "ryeos.hosted-oci-installed-evidence.v1":
        raise ValueError("wrong installed evidence schema")
    if config["installed_attestor"] is not None or evidence["attestation"] is not None:
        raise ValueError("installed attestation is disabled in the source contract")
    claim = evidence["claim_class"]
    if claim not in config["claim_classes"]:
        raise ValueError("unknown qualification claim class")
    for name in ("image_digest", "profile_digest", "policy_digest", "binding_digest", "hook_digest", "lifetime_generation"):
        if not isinstance(evidence[name], str) or not DIGEST.fullmatch(evidence[name]):
            raise ValueError(f"{name} must be an exact digest")
    for name in ("source_revision", "node_fingerprint"):
        value = evidence[name]
        if not isinstance(value, str) or not re.fullmatch(r"[a-f0-9]{40,64}", value):
            raise ValueError(f"{name} must be an exact lowercase identity")
    lifecycle = evidence["lifecycle_identity"]
    if not isinstance(lifecycle, dict) or set(lifecycle) != {
        "host_boot_id", "init_pid", "init_start_time_ticks", "scope_identity"
    }:
        raise ValueError("lifecycle identity is incomplete")
    if not isinstance(lifecycle["host_boot_id"], str) or not lifecycle["host_boot_id"]:
        raise ValueError("host boot identity is absent")
    if any(type(lifecycle[name]) is not int or lifecycle[name] <= 1 for name in ("init_pid", "init_start_time_ticks")):
        raise ValueError("lifecycle process identity is invalid")
    if evidence["controller_account"] != config["runtime_product"]["controller_account"]:
        raise ValueError("controller account does not match the signed runtime product")
    observations = evidence["observations"]
    maximum = config["limits"]["max_observations"]
    if not isinstance(observations, list) or not 1 <= len(observations) <= maximum:
        raise ValueError("observations are absent or exceed the bound")
    if any(not isinstance(item, dict) or set(item) != {"id", "passed", "detail"}
           or not isinstance(item["id"], str) or type(item["passed"]) is not bool
           or not isinstance(item["detail"], str) for item in observations):
        raise ValueError("observation record is invalid")
    observation_ids = [item["id"] for item in observations]
    if len(observation_ids) != len(set(observation_ids)):
        raise ValueError("observation identifiers must be unique")
    capabilities = string_set(evidence["capabilities"], "capabilities")
    refusals = string_set(evidence["refusals"], "refusals")
    required_capabilities = set(config["required_capabilities"])
    required_refusals = set(config["required_refusals"])
    required_observations = set(config["required_observations"])
    passed_observations = {item["id"] for item in observations if item["passed"]}
    installed = claim == "installed_qualification"
    # Shape checking is deliberately insufficient for an installed claim. A
    # future administrator-signed config and verifier backend must authenticate
    # the signature over the canonical evidence payload. Until then source can
    # validate only structural/source records.
    # Structural acceptance means only that this bounded record is well
    # formed. Source-contract execution is established by the repository test
    # runner, not by self-asserted evidence supplied to this tool.
    accepted = claim == "structural_smoke"
    return {
        "schema_version": 1,
        "claim_class": claim,
        "runtime_product": config["runtime_product"],
        "accepted": accepted,
        "missing_capabilities": sorted(required_capabilities - capabilities),
        "missing_refusals": sorted(required_refusals - refusals),
        "missing_observations": sorted(required_observations - passed_observations),
        "installed_attestation_authenticated": False if installed else None,
        "scope": "bounded evidence validation only; no deployment or kernel authority",
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    parser.parse_args()
    raw = sys.stdin.buffer.read(262145)
    if len(raw) > 262144:
        raise ValueError("qualification evidence exceeds the configured bound")
    print(json.dumps(evaluate(json.loads(raw)), sort_keys=True))


if __name__ == "__main__":
    main()
