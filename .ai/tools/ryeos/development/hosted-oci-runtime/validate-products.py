# ryeos:signed:2026-09-17T01:52:20Z:ea666de830b243b4239829d38fd312ad574f718cef03b7280f54a4c8ab7a1484:is3XL7EYjTy0kQ+DmEtKLemMgLA9ix/6HMVrFSks9ickd57qMl8jRRZg1h7Rz5UxII9G+j7WWzv5CZmWvBG2AQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/hosted-oci-runtime
#   version: "1.0.0"
#   description: Validate exact contained-workflow build product evidence
#   executor_id: tool:ryeos/development/authoring-environment-production/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   workspace_access: immutable_current_generation
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties:
#       evidence: {type: object}
#     required: [evidence]
#     additionalProperties: false
#   config_resolve:
#     type: single
#     spec:
#       path: development/ryeos/contained-workflow-products.yaml
#       mode: first_match
"""Validate product coordinates without asserting installed containment."""

import json
import re
import sys

DIGEST = re.compile(r"sha256:[a-f0-9]{64}")
HASH = re.compile(r"[a-f0-9]{64}")
EXPECTED_PRODUCTS = {
    "image": {
        "bake_target": "contained-workflow",
        "docker_target": "ryeos-contained-workflow",
    },
    "host_hook": {
        "bake_target": "contained-oci-hook-artifact",
        "executable": "ryeos-lillux-oci-hook",
    },
    "node_profile": "contained-workflow",
}


def evaluate(request):
    if not isinstance(request, dict) or set(request) != {"resolved_config", "evidence"}:
        raise ValueError("product validation request is not closed")
    config = request["resolved_config"]
    if not isinstance(config, dict) or config.get("schema") != "ryeos.development.contained-workflow-products.v1":
        raise ValueError("wrong contained-workflow product contract")
    if config.get("products") != EXPECTED_PRODUCTS:
        raise ValueError("contained-workflow product coordinates are not exact")
    evidence = request["evidence"]
    required = {"source_revision", "image_digest", "hook_sha256", "profile_sha256"}
    if not isinstance(evidence, dict) or set(evidence) != required:
        raise ValueError("product evidence is not closed")
    if not isinstance(evidence["source_revision"], str) or not re.fullmatch(r"[a-f0-9]{40,64}", evidence["source_revision"]):
        raise ValueError("source revision is not exact")
    if not isinstance(evidence["image_digest"], str) or not DIGEST.fullmatch(evidence["image_digest"]):
        raise ValueError("image digest is not exact")
    for name in ("hook_sha256", "profile_sha256"):
        if not isinstance(evidence[name], str) or not HASH.fullmatch(evidence[name]):
            raise ValueError(f"{name} is not exact")
    return {
        "schema_version": 1,
        "accepted": True,
        "products": EXPECTED_PRODUCTS,
        "evidence": evidence,
        "scope": "immutable product coordinates only; no installed containment claim",
    }


def main():
    raw = sys.stdin.buffer.read(65537)
    if len(raw) > 65536:
        raise ValueError("product evidence exceeds the bounded input")
    print(json.dumps(evaluate(json.loads(raw)), sort_keys=True))


if __name__ == "__main__":
    main()
