# ryeos:signed:2026-11-24T08:38:45Z:9b3f51586f72a066f28b996feb3fbb4c3dd0a7633d771e166df9e77e059a920a:FUCQZimGteVh6480nZ1OwQaAdd/vUIYXNmb+EGydVKMllSNaAFcLf9ChcfEwh5eJJF8lb8GLc6oroIun0o3CCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/core/evaluate
#   version: "1.0.0"
#   description: A candidate evaluator that runs no checks, and says so. It accepts only when told to accept unchecked.
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   workspace_access: immutable_current_generation
#   filesystem_authority: node_policy
#   network_authority: node_policy
#   config_schema:
#     type: object
#     properties:
#       candidate_snapshot_hash: {type: string, pattern: '^[a-f0-9]{64}$'}
#       base_snapshot_hash: {type: string, pattern: '^[a-f0-9]{64}$'}
#       accept_unchecked: {type: boolean}
#     required: [candidate_snapshot_hash, base_snapshot_hash]
#     additionalProperties: false
"""Evaluate one frozen candidate without running any check.

This evaluator exists so a person can accept a candidate they have reviewed
themselves. It is not a test run: it executes nothing from the candidate and
looks at nothing but the two snapshot hashes and whether the candidate view is
a readable directory. It does not accept unless its parameters carry
`accept_unchecked: true`. The parameters are sealed with the evaluator's
launch, so that choice is part of the recorded testimony, and the result's
evidence scope says plainly that no checks ran.
"""

import json
import re
import sys
from pathlib import Path

HASH = re.compile(r"[a-f0-9]{64}")
SCOPE = "no checks run"


def evaluate(request: dict, project: Path) -> dict:
    candidate = request["candidate_snapshot_hash"]
    base = request["base_snapshot_hash"]
    well_formed = bool(HASH.fullmatch(candidate)) and bool(HASH.fullmatch(base))
    differs = candidate != base
    readable = project.is_dir()
    structural = well_formed and differs and readable
    asked = request.get("accept_unchecked") is True
    accepted = structural and asked
    if not structural:
        reason = "the candidate view is not a distinct, readable generation"
    elif not asked:
        reason = "no checks configured"
    else:
        reason = "accepted unchecked at the caller's explicit request"
    return {
        "schema_version": 1,
        "candidate_snapshot_hash": candidate,
        "base_snapshot_hash": base,
        "accepted": accepted,
        "evidence": {
            "scope": SCOPE,
            "reason": reason,
            "accept_unchecked": asked,
            "checks": ["hashes_well_formed", "candidate_differs_from_base", "view_readable"],
        },
    }


def main() -> None:
    request = json.loads(sys.stdin.buffer.read(65537))
    project = Path(sys.argv[2])
    print(json.dumps(evaluate(request, project), sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
