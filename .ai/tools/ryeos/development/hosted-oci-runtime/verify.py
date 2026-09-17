# ryeos:signed:2026-09-17T00:52:58Z:331e6cfa400f5c389b76b48faf8754ff77429458baad1af7ee5cba04e8cbb30f:fAfVsC7UkVKoLFXDa6KCkA7eqadOfc5YfUcnwPhYuzuHCYx5UrNgf/72IYGIhFBrKBMD+LYdHSR5UAZUoVLZAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
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
    "provider_generation", "observations", "capabilities", "refusals",
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
    if config.get("runtime_product") != {
        "image_target": "ryeos-contained-workflow",
        "node_profile": "contained-workflow",
    }:
        raise ValueError("wrong hosted OCI runtime product")
    evidence = request["evidence"]
    if not isinstance(evidence, dict) or set(evidence) != REQUIRED_FIELDS:
        raise ValueError("installed evidence is not a closed current record")
    if evidence["schema"] != "ryeos.hosted-oci-installed-evidence.v1":
        raise ValueError("wrong installed evidence schema")
    claim = evidence["claim_class"]
    if claim not in config["claim_classes"]:
        raise ValueError("unknown qualification claim class")
    for name in ("image_digest", "profile_digest", "policy_digest", "binding_digest", "provider_generation"):
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
    observations = evidence["observations"]
    maximum = config["limits"]["max_observations"]
    if not isinstance(observations, list) or not 1 <= len(observations) <= maximum:
        raise ValueError("observations are absent or exceed the bound")
    if any(not isinstance(item, dict) or set(item) != {"id", "passed", "detail"}
           or not isinstance(item["id"], str) or type(item["passed"]) is not bool
           or not isinstance(item["detail"], str) for item in observations):
        raise ValueError("observation record is invalid")
    capabilities = string_set(evidence["capabilities"], "capabilities")
    refusals = string_set(evidence["refusals"], "refusals")
    required_capabilities = set(config["required_capabilities"])
    required_refusals = set(config["required_refusals"])
    installed = claim == "installed_qualification"
    accepted = (not installed or (required_capabilities <= capabilities
                                  and required_refusals <= refusals
                                  and all(item["passed"] for item in observations)))
    return {
        "schema_version": 1,
        "claim_class": claim,
        "runtime_product": config["runtime_product"],
        "accepted": accepted,
        "missing_capabilities": sorted(required_capabilities - capabilities),
        "missing_refusals": sorted(required_refusals - refusals),
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
