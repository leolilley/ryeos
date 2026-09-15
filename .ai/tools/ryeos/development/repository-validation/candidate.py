# ryeos:signed:2026-09-09T14:55:38Z:c3836b0035dadc8d898cb8baea3a56aabb3431f29469dd4f4a5031c6cc864c2e:4bN6Fkscb+NKT9Dt8u7FTsQOvnl2ws5zqo7v1bJocn1E89NHSq//dso140faC79+8i9N3ndwvhDihq0Oft9gAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/repository-validation
#   version: "1.0.0"
#   description: Independently assert exact configured candidate file hashes without claiming Cargo execution
#   executor_id: tool:ryeos/development/authoring-environment-production/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   workspace_access: immutable_current_generation
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties:
#       task: {type: string, maxLength: 128, pattern: '^[a-z0-9-]+$'}
#       base_snapshot_hash: {type: string, pattern: '^[a-f0-9]{64}$'}
#       candidate_snapshot_hash: {type: string, pattern: '^[a-f0-9]{64}$'}
#     required: [task, base_snapshot_hash, candidate_snapshot_hash]
#     additionalProperties: false
#   config_resolve:
#     type: single
#     spec:
#       path: development/ryeos/candidate-evaluation.yaml
#       mode: first_match
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
"""Independent bounded file assertions, not Cargo or whole-repository proof.

Config comes from admitted resolved_config, never from the candidate's mutable
.ai tree. The candidate-operation owner resolves this Tool from the immutable
base and supplies the read-only candidate workspace. Worker-requested tests
remain separate child evidence, never parameters that can assert test success.
This operation launches no commands and grants no publication authority.
"""

from pathlib import Path
import argparse
import hashlib
import json
import re
import sys

sys.path.insert(0, str(Path(__file__).parent / "lib"))
from validation import read, relative


HASH = re.compile(r"[a-f0-9]{64}")


def selected_task(config, task_id):
    if not isinstance(config, dict) or set(config) != {
        "category", "name", "version", "schema", "limits", "tasks"
    } or config["schema"] != "ryeos.development.candidate-evaluation.v1":
        raise ValueError("invalid candidate assertion config")
    limits = config["limits"]
    if not isinstance(limits, dict) or set(limits) != {
        "max_file_bytes", "max_total_bytes", "max_files"
    } or any(type(value) is not int or value <= 0 for value in limits.values()):
        raise ValueError("candidate assertions require explicit positive bounds")
    if limits["max_files"] > 32:
        raise ValueError("candidate assertion bounds exceed the finite Tool contract")
    tasks = config["tasks"]
    if not isinstance(tasks, dict) or not 1 <= len(tasks) <= 32 or task_id not in tasks:
        raise ValueError("candidate task is not configured")
    task = tasks[task_id]
    if not isinstance(task, dict) or set(task) != {"file_assertions"}:
        raise ValueError("incomplete candidate task")
    assertions = task["file_assertions"]
    if not isinstance(assertions, list) or not 1 <= len(assertions) <= limits["max_files"]:
        raise ValueError("candidate task requires a bounded nonempty file assertion set")
    paths = set()
    for assertion in assertions:
        if not isinstance(assertion, dict) or set(assertion) != {"path", "sha256"}:
            raise ValueError("invalid candidate file assertion")
        relative(assertion["path"])
        if assertion["path"] in paths or not isinstance(assertion["sha256"], str) or not HASH.fullmatch(assertion["sha256"]):
            raise ValueError("candidate assertions require unique paths and exact hashes")
        paths.add(assertion["path"])
    return task, limits


def evaluate(root, request):
    task, limits = selected_task(request["resolved_config"], request["task"])
    base, candidate = request["base_snapshot_hash"], request["candidate_snapshot_hash"]
    if any(not isinstance(value, str) or not HASH.fullmatch(value) for value in (base, candidate)):
        raise ValueError("candidate coordinates must be exact hashes")
    observations, total = [], 0
    for assertion in task["file_assertions"]:
        try:
            # Reuse the source-local validation owner's bounded no-symlink read.
            # Its path checks rely on this operation's frozen immutable view;
            # they are not a race-proof authority for mutable host directories.
            # read() opens binary and uses UTF-8 decode, NOT text-mode universal
            # newline conversion. Re-encoding therefore preserves exact bytes.
            remaining = limits["max_total_bytes"] - total
            if remaining <= 0:
                raise ValueError("candidate assertions exhausted aggregate byte bound")
            data = read(root, assertion["path"], min(limits["max_file_bytes"], remaining)).encode("utf-8")
            total += len(data)
            actual = hashlib.sha256(data).hexdigest()
            observations.append({"path": assertion["path"], "sha256": actual,
                                 "matches": actual == assertion["sha256"]})
        except (OSError, ValueError, UnicodeError):
            observations.append({"path": assertion["path"], "sha256": None, "matches": False})
            # A failed bounded read cannot be treated as zero-byte consumption.
            # Refuse this assessment without touching further candidate files.
            break
    return {
        "schema_version": 1, "base_snapshot_hash": base,
        "candidate_snapshot_hash": candidate,
        "accepted": base != candidate and all(item["matches"] for item in observations),
        "evidence": {"task": request["task"], "files": observations,
                     "scope": "exact configured file hashes only; no Cargo execution or whole-repository correctness claim"},
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True, type=Path)
    args = parser.parse_args()
    if not args.project_path.is_absolute() or args.project_path.is_symlink() or not args.project_path.is_dir():
        raise ValueError("expected exact candidate project root")
    raw = sys.stdin.buffer.read(65537)
    if len(raw) > 65536:
        raise ValueError("candidate assertion request exceeds bounded Tool input")
    print(json.dumps(evaluate(args.project_path, json.loads(raw)), sort_keys=True))


if __name__ == "__main__":
    main()
