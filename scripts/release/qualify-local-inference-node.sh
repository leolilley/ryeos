#!/usr/bin/env bash

# Qualify the exact local-inference artifact contract through a fresh
# disposable RyeOS node. Acquisition is explicit: `--online` proves the
# publisher's configured HTTPS distribution, while `--archive-root` proves the
# generic node-admitted offline archive path. Neither mode falls back to the
# other. Both exercise the installed bundle contract, managed activation, the
# real daemon-owned persistent session, durable banking, restart, and
# zero-contact replay.

set -euo pipefail
export LC_ALL=C

usage() {
    echo "usage: $0 --release-contract PATH --bundle-source DIR --ryeos-bin PATH (--online | --archive-root DIR) [--minimum-free-bytes BYTES] [--trust-file PATH] [--qualification-parent DIR] [--evidence-output PATH] [--keep]" >&2
    exit 2
}

release_contract=""
bundle_source=""
ryeos_bin=""
trust_file=""
qualification_parent="${RUNNER_TEMP:-/tmp}"
online=0
archive_root=""
minimum_free_bytes="2147483648"
keep=0
evidence_output=""
while (($#)); do
    case "$1" in
        --release-contract) release_contract="${2:-}"; shift 2 ;;
        --bundle-source) bundle_source="${2:-}"; shift 2 ;;
        --ryeos-bin) ryeos_bin="${2:-}"; shift 2 ;;
        --trust-file) trust_file="${2:-}"; shift 2 ;;
        --qualification-parent) qualification_parent="${2:-}"; shift 2 ;;
        --online) online=1; shift ;;
        --archive-root) archive_root="${2:-}"; shift 2 ;;
        --minimum-free-bytes) minimum_free_bytes="${2:-}"; shift 2 ;;
        --evidence-output) evidence_output="${2:-}"; shift 2 ;;
        --keep) keep=1; shift ;;
        *) usage ;;
    esac
done

[[ -f "$release_contract" && -d "$bundle_source/.ai" && -x "$ryeos_bin" ]] || usage
[[ -z "$trust_file" || -f "$trust_file" ]] || usage
[[ -d "$qualification_parent" ]] || usage
[[ "$minimum_free_bytes" =~ ^[1-9][0-9]*$ ]] || usage
if [[ "$online" -eq 1 ]]; then
    [[ -z "$archive_root" ]] || usage
    [[ "$minimum_free_bytes" == "2147483648" ]] || {
        echo "online qualification uses the full profile's exact 2147483648-byte free-space floor" >&2
        exit 2
    }
else
    [[ -n "$archive_root" && -d "$archive_root" ]] || usage
fi

release_contract="$(realpath "$release_contract")"
bundle_source="$(realpath "$bundle_source")"
ryeos_bin="$(realpath "$ryeos_bin")"
qualification_parent="$(realpath "$qualification_parent")"
if [[ -n "$evidence_output" ]]; then
    evidence_parent="$(dirname "$evidence_output")"
    [[ -d "$evidence_parent" ]] || usage
    evidence_output="$(cd "$evidence_parent" && pwd)/$(basename "$evidence_output")"
fi
if [[ -n "$archive_root" ]]; then
    archive_root="$(realpath "$archive_root")"
fi

repository_root="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ -n "$trust_file" ]]; then
    trust_file="$(realpath "$trust_file")"
fi
ryeosd_bin="$(dirname "$ryeos_bin")/ryeosd"
[[ -x "$ryeosd_bin" ]] || {
    echo "matching ryeosd is absent beside $ryeos_bin" >&2
    exit 2
}

contract="$bundle_source/../scripts/release/local-inference-qwen3-0.6b-v1.json"
if [[ ! -f "$contract" ]]; then
    contract="$repository_root/scripts/release/local-inference-qwen3-0.6b-v1.json"
fi
cmp "$contract" "$release_contract"

if [[ -n "$archive_root" ]]; then
    python3 - "$contract" "$archive_root" <<'PY'
import hashlib
import json
from pathlib import Path
import stat
import sys

contract = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
root = Path(sys.argv[2])
for item in contract["realizations"]:
    path = root / item["archive"]
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit(f"offline realization is not a regular file: {path}")
    if metadata.st_size > item["maximum_compressed_bytes"]:
        raise SystemExit(f"offline realization exceeds its signed bound: {path}")
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != item["sha256"]:
        raise SystemExit(f"offline realization digest differs: {path}")
PY
fi

qualification_root="$(mktemp -d "$qualification_parent/ryeos-local-inference-node.XXXXXX")"
node_root="$qualification_root/node"
policy_root="$qualification_root/policy"
home_root="$qualification_root/home"
project_root="$qualification_root/project"
uds_path="$qualification_root/ryeosd.sock"
mkdir -p "$policy_root" "$home_root" "$project_root"

# `ryeos init --source` deliberately admits every immediate bundle directory.
# A source checkout also contains separately authored optional bundle sets (for
# example full-sandbox), so pointing init at the checkout would make an
# unrelated optional payload part of this qualification. Build the same closed
# `full` source layout that packaging and local installation derive from the
# shared bundle-set authority. This is a disposable source view, not a second
# bundle selection contract or a lasting node/workload root.
qualification_source="$qualification_root/source"
mkdir -p "$qualification_source"
cp -a "$bundle_source/.ai" "$qualification_source/.ai"
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$repository_root/scripts/pkg/bundle-sets.sh"
while IFS= read -r bundle_name; do
    [[ -d "$bundle_source/$bundle_name/.ai" ]] || {
        echo "full qualification source is missing $bundle_name/.ai" >&2
        exit 2
    }
    mkdir -p "$qualification_source/$bundle_name"
    cp -a "$bundle_source/$bundle_name/.ai" \
        "$qualification_source/$bundle_name/.ai"
    if [[ -f "$bundle_source/$bundle_name/PUBLISHER_TRUST.toml" ]]; then
        cp -a "$bundle_source/$bundle_name/PUBLISHER_TRUST.toml" \
            "$qualification_source/$bundle_name/PUBLISHER_TRUST.toml"
    fi
done < <(ryeos_bundle_set_names full)

node_started=0
cleanup() {
    local status="$1"
    # Raw CLI diagnostics are transient parser input only. They may contain
    # host paths from an unrelated failure and are never qualification
    # evidence, including when a failed node is retained for diagnosis.
    rm -f -- "$qualification_root/.released-binding-launch-refusal.raw"
    if [[ "$node_started" -eq 1 ]]; then
        HOME="$home_root" RYEOS_APP_ROOT="$node_root" \
            PATH="$(dirname "$ryeos_bin"):$PATH" \
            "$ryeos_bin" stop --app-root "$node_root" >/dev/null 2>&1 || true
    fi
    if [[ "$status" -ne 0 || "$keep" -eq 1 ]]; then
        echo "local-inference node qualification retained at $qualification_root" >&2
    else
        rm -rf -- "$qualification_root"
    fi
    return "$status"
}
trap 'cleanup "$?"' EXIT

python3 - "$contract" "$policy_root/external-content.json" \
    "$policy_root/persistent-sessions.json" "$archive_root" \
    "$minimum_free_bytes" <<'PY'
import json
import os
from pathlib import Path
import sys

contract = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
realizations = contract["realizations"]
bounds = [item["bounds"] for item in realizations]
archive_root = sys.argv[4]
minimum_free_bytes = int(sys.argv[5])
if minimum_free_bytes > 2**63 - 1:
    raise SystemExit("minimum-free-bytes exceeds RyeOS's signed integer policy range")
roots = {}
if archive_root:
    identity = os.stat(archive_root, follow_symlinks=False)
    roots["local-inference-archives"] = {
        "path": archive_root,
        "containing_device": identity.st_dev,
        "root_inode": identity.st_ino,
    }
external = {
    "schema": 1,
    "roots": roots,
    "limits": {
        "max_depth": max(item["maximum_depth"] for item in bounds),
        "max_entries": max(item["maximum_entries"] for item in bounds),
        "max_file_bytes": max(item["maximum_file_bytes"] for item in bounds),
        "max_total_bytes": max(item["maximum_total_bytes"] for item in bounds),
        "store_budget_bytes": 4 * 1024**3,
        "minimum_free_bytes": minimum_free_bytes,
    },
    "managed_activation": {
        "enabled": True,
        "limits": {
            "allow_online": not bool(archive_root),
            "allowed_https_hosts": [] if archive_root else [
                "github.com", "release-assets.githubusercontent.com"
            ],
            "max_redirects": 2,
            "max_archives": len(realizations),
            "max_compressed_bytes": sum(
                item["maximum_compressed_bytes"] for item in realizations
            ),
            "max_expanded_bytes": sum(
                item["maximum_expanded_bytes"] for item in realizations
            ),
            "max_members": sum(item["maximum_entries"] for item in realizations),
            "max_member_bytes": max(item["maximum_file_bytes"] for item in bounds),
            "max_concurrent_activations": 1,
            "cache_budget_bytes": 2 * 1024**3,
            "store_budget_bytes": 4 * 1024**3,
            "minimum_free_bytes": minimum_free_bytes,
            "max_attempts": 3,
        },
    },
}
persistent = {
    "schema": 1,
    "enabled": True,
    "limits": {
        "max_pool_groups": 4,
        "max_total_processes": 4,
        "max_total_address_space_bytes": 64 * 1024**3,
        "max_total_cpu_seconds": 14400,
        "max_real_uid_process_limit": 4096,
        "max_open_streams": 32,
        "max_active_streams": 4,
        "max_active_streams_per_subject": 1,
        "max_stream_backlog_bytes": 16 * 1024**2,
        "max_total_backlog_bytes": 64 * 1024**2,
    },
}
Path(sys.argv[2]).write_text(json.dumps(external, indent=2) + "\n", encoding="utf-8")
Path(sys.argv[3]).write_text(json.dumps(persistent, indent=2) + "\n", encoding="utf-8")
PY

# This project is qualification input, not local-inference bundle content. It
# is signed by the disposable node's operator below and then captured through
# the ordinary project authority. The model worker never receives this path or
# any of these tool executables; the directive dispatches each tool as a
# separately admitted RyeOS child.
python3 - "$project_root" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
(root / ".ai/tools/qualification").mkdir(parents=True)
(root / ".ai/directives/qualification").mkdir(parents=True)
(root / ".ai/graphs/qualification").mkdir(parents=True)
(root / ".ai/config/ryeos-runtime").mkdir(parents=True)
(root / "qualification-input.txt").write_text(
    "RYEOS-LOCAL-INPUT-v1\n", encoding="utf-8"
)

tools = {
    "read.py": '''# ryeos-tool:
#   category: qualification
#   version: "1.0.0"
#   tool_type: python
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   description: "Qualification step 1: read the fixed admitted input. Takes no arguments."
#   effects: live
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false

from pathlib import Path

def execute(params: dict, project_path: str) -> dict:
    expected = {
        "cancellation_grace_secs": 5,
        "cancellation_mode": "graceful",
        "project_path": project_path,
        "timeout": 86400,
    }
    if params != expected:
        raise ValueError(f"read received undeclared caller arguments: {params!r}")
    value = Path(project_path, "qualification-input.txt").read_text(encoding="utf-8")
    if value != "RYEOS-LOCAL-INPUT-v1\\n":
        raise ValueError("qualification input differs")
    return {"step": "read", "value": value.strip()}
''',
    "mutate.py": '''# ryeos-tool:
#   category: qualification
#   version: "1.0.0"
#   tool_type: python
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   description: "Qualification step 2: create the fixed candidate after read succeeds. Takes no arguments."
#   effects: live
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false

from pathlib import Path

def execute(params: dict, project_path: str) -> dict:
    expected = {
        "cancellation_grace_secs": 5,
        "cancellation_mode": "graceful",
        "project_path": project_path,
        "timeout": 86400,
    }
    if params != expected:
        raise ValueError(f"mutate received undeclared caller arguments: {params!r}")
    root = Path(project_path)
    if (root / "qualification-input.txt").read_text(encoding="utf-8") != "RYEOS-LOCAL-INPUT-v1\\n":
        raise ValueError("mutation has no admitted input")
    candidate = root / "qualification-candidate.txt"
    if candidate.exists():
        raise ValueError("qualification mutation was attempted more than once")
    candidate.write_text("RYEOS-LOCAL-CANDIDATE-v1\\n", encoding="utf-8")
    return {"step": "mutate", "created": True}
''',
    "verify.py": '''# ryeos-tool:
#   category: qualification
#   version: "1.0.0"
#   tool_type: python
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   description: "Qualification step 3: deterministically verify the admitted input and staged candidate. Takes no arguments."
#   effects: live
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false

from pathlib import Path

def execute(params: dict, project_path: str) -> dict:
    expected = {
        "cancellation_grace_secs": 5,
        "cancellation_mode": "graceful",
        "project_path": project_path,
        "timeout": 86400,
    }
    if params != expected:
        raise ValueError(f"verify received undeclared caller arguments: {params!r}")
    root = Path(project_path)
    observed = {
        "input": (root / "qualification-input.txt").read_text(encoding="utf-8"),
        "candidate": (root / "qualification-candidate.txt").read_text(encoding="utf-8"),
    }
    expected = {
        "input": "RYEOS-LOCAL-INPUT-v1\\n",
        "candidate": "RYEOS-LOCAL-CANDIDATE-v1\\n",
    }
    if observed != expected:
        raise ValueError(f"qualification verification differs: {observed!r}")
    return {"step": "verify", "verified": True}
''',
}
for name, content in tools.items():
    (root / ".ai/tools/qualification" / name).write_text(content, encoding="utf-8")

# The directive runtime already owns bounded in-message tool concurrency through
# its typed project-over-bundle config. Width one is required here because the
# mutation and verification deliberately share the directive's private project
# generation; result ordering alone would not establish mutation ordering.
(root / ".ai/config/ryeos-runtime/execution.yaml").write_text(
    'category: "ryeos-runtime"\ntool_concurrency: 1\n', encoding="utf-8"
)

(root / ".ai/directives/qualification/live_tool_loop.md").write_text(
    '''---
description: "Bounded live Qwen fixture for directive-native RyeOS tool execution."
version: "1.0.0"
model:
  provider: qwen3-0.6b-cpu-4096
  name: qwen3-0.6b
  context_window: 2048
  sampling:
    temperature: 0.0
    seed: 0
  reasoning:
    mode: disabled
effects: recorded
limits:
  turns: 6
  tool_calls: 3
  tokens: 2048
  spend_usd: "0.01"
continuation: false
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.qualification/read
      - ryeos.execute.tool.qualification/mutate
      - ryeos.execute.tool.qualification/verify
---

Run this exact three-step qualification. The exposed function names include the
`qualification_` prefix. Propose the three calls in the exact order below;
RyeOS is configured to dispatch and settle only one call at a time, in order.

1. Call `qualification_read` with `{}`.
2. Call `qualification_mutate` with `{}`.
3. Call `qualification_verify` with `{}`.

After the serial results show `verified: true`, reply with exactly `QUALIFIED` and
nothing else. If any function returns an error, do not claim qualification.
Do not simulate, describe, reorder, repeat, or skip a function call.
''',
    encoding="utf-8",
)

(root / ".ai/graphs/qualification/live_tool_follow.yaml").write_text(
    '''category: qualification
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.qualification/live_tool_loop
      - ryeos.execute.tool.qualification/read
      - ryeos.execute.tool.qualification/mutate
      - ryeos.execute.tool.qualification/verify
config:
  start: qualify
  max_steps: 2
  nodes:
    qualify:
      node_type: action
      follow: true
      action:
        item_id: "directive:qualification/live_tool_loop"
        ref_bindings:
          model: "directive:qualification/live_tool_loop"
        params: {}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
      output:
        qualification: "completed"
''',
    encoding="utf-8",
)
PY

init_args=(
    init --non-interactive --app-root "$node_root" --source "$qualification_source"
    --node-profile full
)
if [[ -n "$trust_file" ]]; then
    init_args+=(--trust-file "$trust_file")
fi
HOME="$home_root" "$ryeos_bin" "${init_args[@]}" >/dev/null
if [[ -n "$archive_root" ]]; then
    HOME="$home_root" "$ryeos_bin" node policy-apply external_content \
        "$policy_root/external-content.json" --app-root "$node_root" --json \
        > "$qualification_root/external-content-policy-result.json"
fi

python3 - "$node_root/.ai/node/policies/external_content.yaml" \
    "$qualification_root/external-policy-evidence.json" \
    "$minimum_free_bytes" <<'PY'
import json
from pathlib import Path
import sys

import yaml

policy_path = Path(sys.argv[1])
policy = yaml.safe_load(policy_path.read_text(encoding="utf-8"))
expected = int(sys.argv[3])
managed = policy["managed_activation"]
if not managed["enabled"] or managed["limits"] is None:
    raise SystemExit("installed policy does not enable managed activation")
if policy["limits"]["minimum_free_bytes"] != expected:
    raise SystemExit("installed external-content free-space floor differs from qualification")
if managed["limits"]["minimum_free_bytes"] != expected:
    raise SystemExit("installed managed-activation free-space floor differs from qualification")
Path(sys.argv[2]).write_text(
    json.dumps(
        {
            "source": str(policy_path),
            "minimum_free_bytes": expected,
            "managed_activation_enabled": True,
        },
        indent=2,
        sort_keys=True,
    ) + "\n",
    encoding="utf-8",
)
PY

cache_root="$node_root/.ai/state/cache/managed-external-content/archives"
[[ ! -e "$cache_root" ]] || {
    echo "fresh qualification node unexpectedly has a managed acquisition cache" >&2
    exit 1
}

start_node() {
    # `ryeos start` may fail after the daemon process has crossed its spawn
    # boundary but before the client receives a successful acknowledgement.
    # Arm cleanup before contact so every ambiguous start is followed by an
    # exact app-root stop attempt.
    node_started=1
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" \
        PATH="$(dirname "$ryeos_bin"):$PATH" \
        "$ryeos_bin" start --app-root "$node_root" \
        --bind 127.0.0.1:0 --uds-path "$uds_path" >/dev/null
}

stop_node() {
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" \
        PATH="$(dirname "$ryeos_bin"):$PATH" \
        "$ryeos_bin" stop --app-root "$node_root" >/dev/null
    node_started=0
}

force_stop_node() {
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" \
        PATH="$(dirname "$ryeos_bin"):$PATH" \
        "$ryeos_bin" stop --force --app-root "$node_root" >/dev/null
    node_started=0
}

worker_pids() {
    python3 - "$node_root" <<'PY'
import os
from pathlib import Path
import sys

root = Path(sys.argv[1]).resolve()
for process in sorted(Path("/proc").iterdir(), key=lambda item: item.name):
    if not process.name.isdecimal():
        continue
    try:
        command = (process / "cmdline").read_bytes()
        cwd = (process / "cwd").resolve(strict=True)
    except (FileNotFoundError, PermissionError):
        continue
    if b"bootstrap.py" not in command:
        continue
    if cwd == root or root in cwd.parents:
        print(process.name)
PY
}

assert_json_path() {
    local result_file="$1"
    local path="$2"
    local expected="$3"
    python3 - "$result_file" "$path" "$expected" <<'PY'
import json
from pathlib import Path
import sys

value = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
path, expected_text = sys.argv[2], sys.argv[3]
try:
    expected = json.loads(expected_text)
except json.JSONDecodeError:
    expected = expected_text
current = value
for component in path.split("."):
    if not isinstance(current, dict) or component not in current:
        raise SystemExit(
            f"{Path(sys.argv[1]).name} has no exact JSON path {path!r}"
        )
    current = current[component]
if current != expected:
    raise SystemExit(
        f"{Path(sys.argv[1]).name} {path!r}: expected={expected!r}, observed={current!r}"
    )
PY
}

assert_json_missing() {
    local result_file="$1"
    local path="$2"
    python3 - "$result_file" "$path" <<'PY'
import json
from pathlib import Path
import sys

value = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
path = sys.argv[2]
current = value
for component in path.split("."):
    if not isinstance(current, dict) or component not in current:
        break
    current = current[component]
else:
    raise SystemExit(f"{Path(sys.argv[1]).name} unexpectedly contains {path!r}")
PY
}

activation_job_id() {
    local result_file="$1"
    python3 - "$result_file" <<'PY'
import json
from pathlib import Path
import sys

value = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
job_id = value.get("job_id")
if not isinstance(job_id, str) or not job_id.startswith("external-activation:"):
    raise SystemExit(f"{Path(sys.argv[1]).name} has no activation job id")
print(job_id)
PY
}

wait_activation_terminal() {
    local job_id="$1"
    local evidence_file="$2"
    # This bounded read-only observer belongs only to the disposable release
    # qualifier. RyeOS itself owns activation through the durable sync-job
    # recovery loop and does not poll its status table as a runtime controller.
    python3 - "$node_root/.ai/state/operational.sqlite3" \
        "$job_id" "$evidence_file" <<'PY'
import json
from pathlib import Path
import sqlite3
import sys
import time

database, job_id, evidence_path = sys.argv[1:4]
deadline = time.monotonic() + 1800
while True:
    connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5)
    connection.row_factory = sqlite3.Row
    try:
        row = connection.execute(
            "SELECT job_id, state, phase, attempt_count, max_attempts, "
            "last_error, finished_at FROM sync_jobs WHERE job_id=?",
            (job_id,),
        ).fetchone()
    finally:
        connection.close()
    if row is None:
        raise SystemExit(f"activation job disappeared: {job_id}")
    observed = dict(row)
    if observed["state"] == "completed":
        Path(evidence_path).write_text(
            json.dumps(observed, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        break
    if observed["state"] in ("failed", "cancelled"):
        raise SystemExit(f"activation terminated without completion: {observed!r}")
    if time.monotonic() >= deadline:
        raise SystemExit(f"activation did not finish within 1800s: {observed!r}")
    time.sleep(2)
PY
}

execution_thread_id() {
    local result_file="$1"
    python3 - "$result_file" <<'PY'
import json
from pathlib import Path
import sys

value = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
thread_id = value.get("thread_id")
if not isinstance(thread_id, str) or not thread_id:
    raise SystemExit(f"{Path(sys.argv[1]).name} has no execution thread id")
print(thread_id)
PY
}

unique_thread_id_for_item() {
    local result_file="$1"
    local item_ref="$2"
    local expected_status="$3"
    python3 - "$result_file" "$item_ref" "$expected_status" <<'PY'
import json
from pathlib import Path
import sys

value = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
item_ref, expected_status = sys.argv[2:4]
threads = value.get("threads", [])
matches = [
    item for item in threads
    if item.get("item_ref") == item_ref and item.get("status") == expected_status
]
if len(matches) != 1:
    raise SystemExit(
        f"expected one {expected_status} thread for {item_ref}, observed {matches!r}"
    )
thread_id = matches[0].get("thread_id")
if not isinstance(thread_id, str) or not thread_id:
    raise SystemExit(f"thread list has no exact id for {item_ref}")
print(thread_id)
PY
}

thread_service() {
    local service_ref="$1"
    local request_json="$2"
    local result_file="$3"
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
        "$service_ref" --no-project --no-stream --input "$request_json" \
        > "$result_file"
}

tail_exact_thread() {
    local thread_id="$1"
    local evidence_file="$2"
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
        service:threads/tail --no-project --no-stream \
        --input "{\"thread_id\":\"$thread_id\",\"thread_only\":true}" \
        > "$evidence_file"
}

follow_child_thread_id() {
    local parent_file="$1"
    python3 - "$parent_file" <<'PY'
import json
from pathlib import Path
import sys

value = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
follow = value.get("thread", {}).get("follow")
thread_id = follow.get("child_thread_id") if isinstance(follow, dict) else None
if not isinstance(thread_id, str) or not thread_id:
    raise SystemExit("suspended graph parent has no exact followed child")
print(thread_id)
PY
}

follow_successor_thread_id() {
    local parent_thread_id="$1"
    local chain_file="$2"
    python3 - "$parent_thread_id" "$chain_file" <<'PY'
import json
from pathlib import Path
import sys

parent = sys.argv[1]
value = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
threads = value.get("threads", [])
successors = [
    item for item in threads
    if item.get("thread_id") != parent
    and item.get("upstream_thread_id") == parent
]
if len(successors) != 1:
    raise SystemExit(1)
thread_id = successors[0].get("thread_id")
if not isinstance(thread_id, str) or not thread_id:
    raise SystemExit(1)
print(thread_id)
PY
}

verify_managed_cache() {
    local evidence_file="$1"
    python3 - "$contract" "$cache_root" "$evidence_file" <<'PY'
import hashlib
import json
from pathlib import Path
import sys

contract = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
cache = Path(sys.argv[2])
expected = {item["sha256"]: item for item in contract["realizations"]}
observed = {path.name: path for path in cache.iterdir() if path.is_file()}
if set(observed) != set(expected):
    raise SystemExit(
        f"managed acquisition cache differs from exact source closure: "
        f"expected={sorted(expected)}, observed={sorted(observed)}"
    )
evidence = []
for digest, item in sorted(expected.items()):
    path = observed[digest]
    actual = hashlib.file_digest(path.open("rb"), "sha256").hexdigest()
    if actual != digest:
        raise SystemExit(f"managed cache entry {digest} verifies as {actual}")
    evidence.append({
        "archive": item["archive"],
        "sha256": digest,
        "bytes": path.stat().st_size,
    })
Path(sys.argv[3]).write_text(
    json.dumps(evidence, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY
}

snapshot_provider_bank() {
    local evidence_file="$1"
    local expected_count="$2"
    python3 - "$node_root" "$evidence_file" "$expected_count" "$qualification_root" <<'PY'
import json
from pathlib import Path
import sqlite3
import sys

node = Path(sys.argv[1])
expected_count = int(sys.argv[3])
qualification_root = Path(sys.argv[4])
accounting_path = node / ".ai/state/accounting.sqlite3"
operational_path = node / ".ai/state/operational.sqlite3"
generation = json.loads(
    (node / ".ai/state/recovery/thread-projection/generation.json").read_text(
        encoding="utf-8"
    )
)
projection_path = node / ".ai/state" / generation["projection_file"]

def rows(path, query, parameters=()):
    connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    try:
        return [dict(row) for row in connection.execute(query, parameters)]
    finally:
        connection.close()

attempts = rows(
    accounting_path,
    "SELECT * FROM provider_attempt_reservation ORDER BY attempt_id",
)
operations = rows(
    accounting_path,
    "SELECT * FROM accounting_operation "
    "ORDER BY attempt_id, operation_kind, transition_sequence",
)
records = rows(
    operational_path,
    "SELECT cache_key, answer_digest, record_hash, produced_at, last_replayed_at "
    "FROM replay_records WHERE namespace='provider.call' ORDER BY cache_key",
)

def cas_object(digest):
    if not isinstance(digest, str) or len(digest) != 64:
        raise SystemExit(f"invalid retained object digest: {digest!r}")
    path = (
        node / ".ai/state/objects/objects" / digest[:2] / digest[2:4]
        / f"{digest}.json"
    )
    return json.loads(path.read_text(encoding="utf-8"))

provider_calls = [cas_object(record["record_hash"]) for record in records]
observation_rows = rows(
    projection_path,
    "SELECT e.event_hash, e.chain_root_id, e.thread_id, e.thread_seq, "
    "e.durability, e.payload, t.status AS thread_status "
    "FROM events e JOIN threads t ON t.thread_id=e.thread_id "
    "WHERE e.event_type='provider_call_observation_recorded' "
    "ORDER BY e.event_id",
)
observations = []
for item in observation_rows:
    payload = item.pop("payload")
    if isinstance(payload, bytes):
        payload = payload.decode("utf-8")
    item["payload"] = json.loads(payload)
    observations.append(item)
if len(attempts) != expected_count:
    raise SystemExit(
        f"expected {expected_count} provider attempts, observed {len(attempts)}"
    )
expected_attempt = {
    "state": "reconciled",
    "reserved_usd_nanos": 0,
    "budget_charge_usd_nanos": 0,
    "provider_actual_usd_nanos": 0,
    "charge_basis": "explicitly_free",
    "reconciliation_reason": "explicitly_free_contract",
}
for attempt in attempts:
    for field, expected in expected_attempt.items():
        if attempt[field] != expected:
            raise SystemExit(
                f"provider attempt {field}: expected={expected!r}, observed={attempt[field]!r}"
            )
    if attempt["settled_at_ms"] is None:
        raise SystemExit("provider attempt is terminal without settled_at_ms")
for field in ("launch_generation", "config_hash", "authority_digest"):
    if len({attempt[field] for attempt in attempts}) != expected_count:
        raise SystemExit(f"exact provider attempts share {field}")
publication = [
    item for item in operations
    if item["operation_kind"] == "provider_call_publication"
    and item["transition_sequence"] == 1
]
if len(publication) != expected_count:
    raise SystemExit(
        f"expected {expected_count} provider_call_publication operations, observed {len(publication)}"
    )
if len(records) != expected_count:
    raise SystemExit(
        f"expected {expected_count} provider replay records, observed {len(records)}"
    )
expected_workers = {
    "worker:local-inference/qwen3-0.6b-cpu-4096",
    "worker:local-inference/qwen3-0.6b-cpu-2048",
}
workers = {
    record["coordinate"]["transport"].get("worker_ref")
    for record in provider_calls
}
if workers != expected_workers:
    raise SystemExit(f"provider coordinates name the wrong exact workers: {workers!r}")
expected_profiles = {
    "qwen3-0.6b-cpu-4096": "worker:local-inference/qwen3-0.6b-cpu-4096",
    "qwen3-0.6b-cpu-2048": "worker:local-inference/qwen3-0.6b-cpu-2048",
}
expected_threads = {
    profile: json.loads(
        (qualification_root / f"executed-{profile.rsplit('-', 1)[-1]}.json").read_text(
            encoding="utf-8"
        )
    )["thread_id"]
    for profile in expected_profiles
}
attempts_by_id = {item["attempt_id"]: item for item in attempts}
if len(attempts_by_id) != len(attempts):
    raise SystemExit("provider attempt ids are not unique")
for record in provider_calls:
    if record.get("cache_key") not in {item["cache_key"] for item in records}:
        raise SystemExit("provider-call object is absent from the replay index")
    transport = record["coordinate"]["transport"]
    provider_id = record["coordinate"]["provider_id"]
    expected_worker = expected_profiles.get(provider_id)
    if expected_worker is None or transport.get("worker_ref") != expected_worker:
        raise SystemExit(
            "provider coordinate does not bind the exact profile/worker pair: "
            f"provider={provider_id!r}, worker={transport.get('worker_ref')!r}"
        )
    observation = record["first_observation"]
    attempt = attempts_by_id.get(observation.get("attempt_id"))
    if attempt is None:
        raise SystemExit("provider first observation names no accounting attempt")
    expected_thread = expected_threads[provider_id]
    exact_attempt_fields = {
        "provider_id": provider_id,
        "config_hash": record["coordinate"]["provider_config_hash"],
        "authority_digest": record["coordinate"]["authority_digest"],
        "thread_id": expected_thread,
    }
    for field, expected in exact_attempt_fields.items():
        if attempt[field] != expected:
            raise SystemExit(
                f"provider {provider_id} attempt {field}: "
                f"expected={expected!r}, observed={attempt[field]!r}"
            )
    if observation.get("produced_by_thread") != expected_thread:
        raise SystemExit(
            f"provider {provider_id} observation was produced by the wrong execution thread"
        )
    for field in (
        "effective_definition_digest",
        "capsule_hash",
        "execution_realization_hash",
    ):
        value = transport.get(field)
        if not isinstance(value, str) or len(value) != 64:
            raise SystemExit(f"provider coordinate has no exact {field}")
for field in (
    "effective_definition_digest",
    "capsule_hash",
    "execution_realization_hash",
):
    if len({record["coordinate"]["transport"][field] for record in provider_calls}) != expected_count:
        raise SystemExit(f"exact worker profiles share {field}")
for field in ("provider_config_hash", "provider_config_value_digest"):
    if len({record["coordinate"][field] for record in provider_calls}) != expected_count:
        raise SystemExit(f"exact worker profiles share provider coordinate field {field}")
if len(
    {record["coordinate"]["outer_effective_definition_digest"] for record in provider_calls}
) != expected_count:
    raise SystemExit("exact worker profiles share outer effective program identity")
observation_coordinates = {
    observation["payload"]["effect_coordinate_digest"]
    for observation in observations
}
if observation_coordinates != {record["cache_key"] for record in provider_calls}:
    raise SystemExit("durable provider observations contradict exact call coordinates")
records_by_hash = {record["record_hash"]: record for record in records}
for operation in publication:
    proof = json.loads(operation["response_json"])
    record = records_by_hash.get(proof.get("record_hash"))
    if record is None:
        raise SystemExit("provider publication names no retained replay record")
    for field in ("cache_key", "answer_digest", "record_hash"):
        if proof.get(field) != record[field]:
            raise SystemExit(
                f"provider publication proof {field} contradicts replay record"
            )
evidence = {
    "attempts": attempts,
    "operations": operations,
    "provider_records": records,
    "provider_call_objects": sorted(
        provider_calls,
        key=lambda record: record["coordinate"]["transport"]["worker_ref"],
    ),
    "provider_observations": observations,
}
Path(sys.argv[2]).write_text(
    json.dumps(evidence, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY
}

start_node

# Static admission must run the signed launch preparer and identify both exact
# workers before activation, while creating no thread, worker, session, lease,
# reservation, or content publication.
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-before-validation.json"
for profile in qwen3-0.6b-cpu-4096 qwen3-0.6b-cpu-2048; do
    directive="directive:local-inference/examples/${profile//[-.]/_}_smoke"
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" validate \
        "$directive" --ref-binding "model=$directive" --no-project --input '{}' \
        > "$qualification_root/validation-before-$profile.json"
done
[[ -z "$(worker_pids)" ]] || {
    echo "static validation launched a local-inference worker" >&2
    exit 1
}
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-after-validation.json"
python3 - "$qualification_root" before <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])

def thread_ids(path):
    value = json.loads(path.read_text())
    found = set()
    def walk(item):
        if isinstance(item, dict):
            thread_id = item.get("thread_id")
            if isinstance(thread_id, str):
                found.add(thread_id)
            for child in item.values():
                walk(child)
        elif isinstance(item, list):
            for child in item:
                walk(child)
    walk(value)
    return found

before = thread_ids(root / "threads-before-validation.json")
after = thread_ids(root / "threads-after-validation.json")
if before != after:
    raise SystemExit(f"static validation changed thread inventory: {before} -> {after}")

resolution_identities = []
for profile in ("qwen3-0.6b-cpu-4096", "qwen3-0.6b-cpu-2048"):
    value = json.loads((root / f"validation-before-{profile}.json").read_text())
    def find(item):
        if isinstance(item, dict):
            if "runtime_preparation" in item and "admission_ready" in item:
                return item
            for child in item.values():
                found = find(child)
                if found is not None:
                    return found
        elif isinstance(item, list):
            for child in item:
                found = find(child)
                if found is not None:
                    return found
        return None
    result = find(value)
    if result is None:
        raise SystemExit(f"validation omitted runtime preparation for {profile}")
    dependencies = result["runtime_preparation"]["execution_dependencies"]
    if len(dependencies) != 1:
        raise SystemExit(f"validation selected {len(dependencies)} workers for {profile}")
    dependency = next(iter(dependencies.values()))
    if dependency["canonical_ref"] != f"worker:local-inference/{profile}":
        raise SystemExit(f"validation selected wrong worker for {profile}: {dependency!r}")
    pins = dependency["external_content"]["declarations"]
    if {item["id"] for item in pins} != {"runtime", "tinygrad", "toolchain", "model"}:
        raise SystemExit(f"validation omitted exact pins for {profile}")
    credentials = result["runtime_preparation"].get("credential_readiness")
    if credentials != {"status": "required_none", "required_count": 0}:
        raise SystemExit(f"credential-free profile projected credential access: {credentials!r}")
    resolution_identities.append(
        json.dumps(dependency["resolution"], sort_keys=True, separators=(",", ":"))
    )
    if result["admission_ready"] or dependency["admission_ready"]:
        raise SystemExit(f"unactivated profile {profile} was reported ready")
if len(set(resolution_identities)) != 2:
    raise SystemExit("the two exact worker profiles share a dependency resolution identity")
PY

activation_args=(
    external-content activate
    config:ryeos-runtime/qwen3-0.6b-cpu-4096-activation
)
if [[ -n "$archive_root" ]]; then
    activation_args+=(offline local-inference-archives)
else
    activation_args+=(online)
fi
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    "${activation_args[@]}" \
    > "$qualification_root/activation-first.json"
assert_json_path "$qualification_root/activation-first.json" state running
assert_json_path "$qualification_root/activation-first.json" idempotent false
assert_json_missing "$qualification_root/activation-first.json" receipt_hash
activation_job="$(activation_job_id "$qualification_root/activation-first.json")"

# Prove that the promptly returned coordinate is real recovery authority, not
# an in-process future. The disposable node is stopped forcefully while the
# first exact attempt is live, then startup reconciliation claims the same job.
force_stop_node
start_node
wait_activation_terminal "$activation_job" \
    "$qualification_root/activation-terminal-db.json"
thread_service service:sync/jobs/inspect \
    "{\"job_id\":\"$activation_job\"}" \
    "$qualification_root/activation-job-inspect.json"
assert_json_path "$qualification_root/activation-job-inspect.json" status found
assert_json_path "$qualification_root/activation-job-inspect.json" job.state completed
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    "${activation_args[@]}" \
    > "$qualification_root/activation-idempotent.json"
assert_json_path "$qualification_root/activation-idempotent.json" idempotent true
assert_json_path "$qualification_root/activation-idempotent.json" state completed
python3 - "$qualification_root/activation-job-inspect.json" \
    "$qualification_root/activation-idempotent.json" <<'PY'
import json
from pathlib import Path
import sys

inspection = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
completion = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
attempts = inspection["attempts"]
if len(attempts) != 2:
    raise SystemExit(f"restart-recovered activation has {len(attempts)} attempts, expected 2")
first, second = attempts
if first["state"] != "failed" or "daemon restarted" not in (first["error"] or ""):
    raise SystemExit(f"first activation attempt is not restart testimony: {first!r}")
if second["state"] != "completed":
    raise SystemExit(f"recovered activation attempt did not complete: {second!r}")
job = inspection["job"]
if job["attempt_count"] != 2:
    raise SystemExit("public sync-job result has the wrong attempt count")
stored = job["result"]
if stored.get("idempotent") is not False or completion.get("idempotent") is not True:
    raise SystemExit("activation did not distinguish publication from idempotent reuse")
for field in ("job_id", "activation_id", "consumer_ref", "state", "receipt_hash"):
    if stored.get(field) != completion.get(field):
        raise SystemExit(f"public sync-job result contradicts reuse field {field}")
if attempts[-1].get("result") != stored:
    raise SystemExit("completed activation attempt contradicts its durable job result")
receipt_hash = completion.get("receipt_hash")
if not isinstance(receipt_hash, str) or len(receipt_hash) != 64:
    raise SystemExit("completed activation has no canonical receipt hash")
PY
verify_managed_cache "$qualification_root/managed-cache-first.json"
verify_managed_cache "$qualification_root/managed-cache-idempotent.json"
cmp "$qualification_root/managed-cache-first.json" \
    "$qualification_root/managed-cache-idempotent.json"

activation_args=(
    external-content activate
    config:ryeos-runtime/qwen3-0.6b-cpu-2048-activation
)
if [[ -n "$archive_root" ]]; then
    activation_args+=(offline local-inference-archives)
else
    activation_args+=(online)
fi
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    "${activation_args[@]}" \
    > "$qualification_root/activation-2048-first.json"
assert_json_path "$qualification_root/activation-2048-first.json" state running
assert_json_path "$qualification_root/activation-2048-first.json" idempotent false
activation_2048_job="$(activation_job_id "$qualification_root/activation-2048-first.json")"
wait_activation_terminal "$activation_2048_job" \
    "$qualification_root/activation-2048-terminal-db.json"
thread_service service:sync/jobs/inspect \
    "{\"job_id\":\"$activation_2048_job\"}" \
    "$qualification_root/activation-2048-job-inspect.json"
assert_json_path "$qualification_root/activation-2048-job-inspect.json" job.state completed
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    "${activation_args[@]}" \
    > "$qualification_root/activation-2048-idempotent.json"
assert_json_path "$qualification_root/activation-2048-idempotent.json" idempotent true
assert_json_path "$qualification_root/activation-2048-idempotent.json" state completed

# Both exact consumer bindings now exist. Re-run threadless validation and
# prove readiness without creating an execution thread or resident worker.
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-before-ready-validation.json"
for profile in qwen3-0.6b-cpu-4096 qwen3-0.6b-cpu-2048; do
    directive="directive:local-inference/examples/${profile//[-.]/_}_smoke"
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" validate \
        "$directive" --ref-binding "model=$directive" --no-project --input '{}' \
        > "$qualification_root/validation-after-$profile.json"
done
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-after-ready-validation.json"
[[ -z "$(worker_pids)" ]] || {
    echo "ready static validation launched a local-inference worker" >&2
    exit 1
}
python3 - "$qualification_root" <<'PY'
import hashlib
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])

def thread_ids(path):
    value = json.loads(path.read_text())
    found = set()
    def walk(item):
        if isinstance(item, dict):
            if isinstance(item.get("thread_id"), str):
                found.add(item["thread_id"])
            for child in item.values():
                walk(child)
        elif isinstance(item, list):
            for child in item:
                walk(child)
    walk(value)
    return found

if thread_ids(root / "threads-before-ready-validation.json") != thread_ids(
    root / "threads-after-ready-validation.json"
):
    raise SystemExit("ready static validation changed thread inventory")

resolution_identities = []
for profile in ("qwen3-0.6b-cpu-4096", "qwen3-0.6b-cpu-2048"):
    value = json.loads((root / f"validation-after-{profile}.json").read_text())
    def find(item):
        if isinstance(item, dict):
            if "runtime_preparation" in item and "admission_ready" in item:
                return item
            for child in item.values():
                found = find(child)
                if found is not None:
                    return found
        elif isinstance(item, list):
            for child in item:
                found = find(child)
                if found is not None:
                    return found
        return None
    result = find(value)
    if result is None or not result["admission_ready"]:
        raise SystemExit(f"activated profile {profile} was not ready: {value!r}")
    dependency = next(iter(result["runtime_preparation"]["execution_dependencies"].values()))
    if not dependency["admission_ready"]:
        raise SystemExit(f"activated dependency {profile} was not ready")
    pins = dependency["external_content"]["declarations"]
    if any(item["status"] != "ready" or item["binding_digest"] is None for item in pins):
        raise SystemExit(f"activated dependency {profile} lacks exact binding readiness")
    credentials = result["runtime_preparation"].get("credential_readiness")
    if credentials != {"status": "required_none", "required_count": 0}:
        raise SystemExit(f"credential-free profile projected credential access: {credentials!r}")
    resolution_identities.append(
        json.dumps(dependency["resolution"], sort_keys=True, separators=(",", ":"))
    )
    if profile == "qwen3-0.6b-cpu-4096":
        model_pin = next(item for item in pins if item["id"] == "model")
        seed = {
            "manifest_hash": model_pin["expected_digest"],
            "consumer_ref": dependency["canonical_ref"],
            "publisher_fingerprint": dependency["resolution"]["root"]
            ["signer_fingerprint"],
        }
        canonical = json.dumps(seed, sort_keys=True, separators=(",", ":"))
        (root / "release-binding-id-4096.txt").write_text(
            hashlib.sha256(canonical.encode()).hexdigest() + "\n",
            encoding="utf-8",
        )
if len(set(resolution_identities)) != 2:
    raise SystemExit("activated exact profiles share a dependency resolution identity")
PY

HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    --project "$project_root" sign \
    tool:qualification/read \
    tool:qualification/mutate \
    tool:qualification/verify \
    config:ryeos-runtime/execution \
    directive:qualification/live_tool_loop \
    graph:qualification/live_tool_follow \
    > "$qualification_root/project-signing.json"

HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
    directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
    --ref-binding model=directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
    --no-project --no-stream --input '{}' > "$qualification_root/executed-4096.json"
assert_json_path "$qualification_root/executed-4096.json" status completed
assert_json_path "$qualification_root/executed-4096.json" success true
assert_json_path "$qualification_root/executed-4096.json" result OK
assert_json_missing "$qualification_root/executed-4096.json" error
worker_pids > "$qualification_root/worker-pids-4096.txt"
[[ -s "$qualification_root/worker-pids-4096.txt" ]] || {
    echo "4096 profile left no resident admitted worker" >&2
    exit 1
}

stop_node
[[ -z "$(worker_pids)" ]] || {
    echo "4096 profile worker survived the proved node stop" >&2
    exit 1
}
start_node
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
    directive:local-inference/examples/qwen3_0_6b_cpu_2048_smoke \
    --ref-binding model=directive:local-inference/examples/qwen3_0_6b_cpu_2048_smoke \
    --no-project --no-stream --input '{}' > "$qualification_root/executed-2048.json"
assert_json_path "$qualification_root/executed-2048.json" status completed
assert_json_path "$qualification_root/executed-2048.json" success true
assert_json_path "$qualification_root/executed-2048.json" result OK
assert_json_missing "$qualification_root/executed-2048.json" error
worker_pids > "$qualification_root/worker-pids-2048.txt"
[[ -s "$qualification_root/worker-pids-2048.txt" ]] || {
    echo "2048 profile left no resident admitted worker" >&2
    exit 1
}
stop_node
[[ -z "$(worker_pids)" ]] || {
    echo "2048 profile worker survived the proved node stop" >&2
    exit 1
}
snapshot_provider_bank "$qualification_root/bank-before-replay.json" 2
python3 - "$policy_root/persistent-sessions.json" <<'PY'
import json
from pathlib import Path
import sys

path = Path(sys.argv[1])
policy = json.loads(path.read_text(encoding="utf-8"))
policy["limits"]["max_total_address_space_bytes"] = 1
path.write_text(json.dumps(policy, indent=2) + "\n", encoding="utf-8")
PY
HOME="$home_root" "$ryeos_bin" node policy-apply persistent_sessions \
    "$policy_root/persistent-sessions.json" --app-root "$node_root" --json \
    > "$qualification_root/replay-zero-capacity-policy-result.json"
start_node
[[ -z "$(worker_pids)" ]] || {
    echo "daemon restart contacted local inference before a request" >&2
    exit 1
}
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-before-refusal-validation.json"
for profile in qwen3-0.6b-cpu-4096 qwen3-0.6b-cpu-2048; do
    directive="directive:local-inference/examples/${profile//[-.]/_}_smoke"
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" validate \
        "$directive" --ref-binding "model=$directive" --no-project --input '{}' \
        > "$qualification_root/validation-refused-$profile.json"
done
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-after-refusal-validation.json"
[[ -z "$(worker_pids)" ]] || {
    echo "refusal validation contacted a local-inference worker" >&2
    exit 1
}
python3 - "$qualification_root" <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])

def thread_ids(path):
    value = json.loads(path.read_text())
    found = set()
    def walk(item):
        if isinstance(item, dict):
            if isinstance(item.get("thread_id"), str):
                found.add(item["thread_id"])
            for child in item.values():
                walk(child)
        elif isinstance(item, list):
            for child in item:
                walk(child)
    walk(value)
    return found

if thread_ids(root / "threads-before-refusal-validation.json") != thread_ids(
    root / "threads-after-refusal-validation.json"
):
    raise SystemExit("refusal validation changed thread inventory")

for profile in ("qwen3-0.6b-cpu-4096", "qwen3-0.6b-cpu-2048"):
    value = json.loads((root / f"validation-refused-{profile}.json").read_text())
    def find(item):
        if isinstance(item, dict):
            if "runtime_preparation" in item and "admission_ready" in item:
                return item
            for child in item.values():
                found = find(child)
                if found is not None:
                    return found
        elif isinstance(item, list):
            for child in item:
                found = find(child)
                if found is not None:
                    return found
        return None
    result = find(value)
    dependency = next(iter(result["runtime_preparation"]["execution_dependencies"].values()))
    session = dependency["session"]
    if result["admission_ready"] or dependency["admission_ready"]:
        raise SystemExit(f"refusal policy admitted profile {profile}")
    if (
        session["ready_for_admission"]
        or session["status"] != "resource_request_exceeds_policy"
        or "reason" in session
    ):
        raise SystemExit(f"refusal policy was not projected for {profile}: {session!r}")
PY
python3 - "$qualification_root/bank-before-replay.json" <<'PY'
import datetime
import json
from pathlib import Path
import sys
import time

bank = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
recorded = max(
    datetime.datetime.fromisoformat(
        record["last_replayed_at"].replace("Z", "+00:00")
    )
    for record in bank["provider_records"]
)
target = recorded + datetime.timedelta(seconds=1)
deadline = time.monotonic() + 2.0
while datetime.datetime.now(datetime.timezone.utc) < target:
    if time.monotonic() >= deadline:
        raise SystemExit("clock did not advance beyond replay timestamp precision")
    time.sleep(0.05)
PY
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
    directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
    --ref-binding model=directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
    --no-project --no-stream --input '{}' > "$qualification_root/replayed-4096.json"
assert_json_path "$qualification_root/replayed-4096.json" status completed
assert_json_path "$qualification_root/replayed-4096.json" success true
assert_json_path "$qualification_root/replayed-4096.json" result OK
assert_json_missing "$qualification_root/replayed-4096.json" error
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
    directive:local-inference/examples/qwen3_0_6b_cpu_2048_smoke \
    --ref-binding model=directive:local-inference/examples/qwen3_0_6b_cpu_2048_smoke \
    --no-project --no-stream --input '{}' > "$qualification_root/replayed-2048.json"
assert_json_path "$qualification_root/replayed-2048.json" status completed
assert_json_path "$qualification_root/replayed-2048.json" success true
assert_json_path "$qualification_root/replayed-2048.json" result OK
assert_json_missing "$qualification_root/replayed-2048.json" error
[[ -z "$(worker_pids)" ]] || {
    echo "provider replay spawned or contacted a local-inference worker" >&2
    exit 1
}
stop_node
snapshot_provider_bank "$qualification_root/bank-after-replay.json" 2

python3 - "$qualification_root" <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])
before = json.loads((root / "bank-before-replay.json").read_text())
after = json.loads((root / "bank-after-replay.json").read_text())
if before["attempts"] != after["attempts"]:
    raise SystemExit("provider replay changed or created an accounting reservation")
if before["operations"] != after["operations"]:
    raise SystemExit("provider replay changed or created an accounting operation")
before_records = {item["cache_key"]: item for item in before["provider_records"]}
after_records = {item["cache_key"]: item for item in after["provider_records"]}
if set(before_records) != set(after_records) or len(before_records) != 2:
    raise SystemExit("provider replay record identity set changed")
for cache_key, before_record in before_records.items():
    after_record = after_records[cache_key]
    for field in ("cache_key", "answer_digest", "record_hash", "produced_at"):
        if before_record[field] != after_record[field]:
            raise SystemExit(f"provider replay changed immutable replay field {field}")
    if after_record["last_replayed_at"] <= before_record["last_replayed_at"]:
        raise SystemExit("provider replay did not advance replay retention evidence")

first_observations = before["provider_observations"]
final_observations = after["provider_observations"]
if len(first_observations) != 2 or len(final_observations) != 4:
    raise SystemExit(
        "expected exactly two executed and two replayed provider observations"
    )
if final_observations[:2] != first_observations:
    raise SystemExit("provider replay changed a terminal executed observation")
observations_by_hash = {}
for item in final_observations:
    observations_by_hash.setdefault(item["payload"].get("record_hash"), []).append(item)
attempt_ids = {item["attempt_id"] for item in before["attempts"]}
for record in before_records.values():
    pair = observations_by_hash.get(record["record_hash"], [])
    if len(pair) != 2:
        raise SystemExit("provider record does not have one execution and one replay")
    executed, replayed = pair
    for item in pair:
        if item["durability"] != "durable" or item["thread_status"] != "completed":
            raise SystemExit("provider observation is not attached to a completed durable thread")
        if item["payload"]["answer_digest"] != record["answer_digest"]:
            raise SystemExit("provider observation contradicts the banked answer digest")
    if executed["payload"]["source"] != "executed":
        raise SystemExit("first provider observation is not executed evidence")
    if executed["payload"]["publication"] not in ("inserted", "folded"):
        raise SystemExit("first provider observation did not confirm publication")
    if executed["payload"].get("replayed_from") is not None:
        raise SystemExit("executed provider observation falsely names a replay source")
    if replayed["payload"]["source"] != "replay":
        raise SystemExit("second provider observation is not replay evidence")
    if replayed["payload"]["publication"] != "not_applicable":
        raise SystemExit("replayed provider observation falsely claims publication")
    replay_source = replayed["payload"].get("replayed_from")
    if not isinstance(replay_source, dict):
        raise SystemExit("replayed provider observation omits its exact source")
    if replay_source.get("produced_by_thread") != executed["thread_id"]:
        raise SystemExit("replayed provider observation names the wrong source thread")
    if replay_source.get("attempt_id") not in attempt_ids:
        raise SystemExit("replayed provider observation names an unknown source attempt")
PY

# Restore the actual session ceiling and qualify the directive-native tool
# path. The preceding zero-capacity phase already proved exact provider replay
# before worker contact; this phase deliberately performs new useful local work
# in a pinned private project generation.
python3 - "$policy_root/persistent-sessions.json" <<'PY'
import json
from pathlib import Path
import sys

path = Path(sys.argv[1])
policy = json.loads(path.read_text(encoding="utf-8"))
policy["limits"]["max_total_address_space_bytes"] = 64 * 1024**3
path.write_text(json.dumps(policy, indent=2) + "\n", encoding="utf-8")
PY
HOME="$home_root" "$ryeos_bin" node policy-apply persistent_sessions \
    "$policy_root/persistent-sessions.json" --app-root "$node_root" --json \
    > "$qualification_root/live-tool-policy-result.json"
start_node
[[ -z "$(worker_pids)" ]] || {
    echo "capacity restoration contacted local inference before a request" >&2
    exit 1
}

HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    --project "$project_root" validate \
    directive:qualification/live_tool_loop \
    --ref-binding model=directive:qualification/live_tool_loop \
    '{}' > "$qualification_root/live-tool-validation.json"
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    --project "$project_root" execute \
    directive:qualification/live_tool_loop \
    --ref-binding model=directive:qualification/live_tool_loop \
    --pin-project --no-stream '{}' \
    > "$qualification_root/live-tool-executed.json"
assert_json_path "$qualification_root/live-tool-executed.json" status completed
assert_json_path "$qualification_root/live-tool-executed.json" success true
assert_json_path "$qualification_root/live-tool-executed.json" result QUALIFIED
assert_json_missing "$qualification_root/live-tool-executed.json" error
[[ ! -e "$project_root/qualification-candidate.txt" ]] || {
    echo "live tool mutation escaped the pinned private generation" >&2
    exit 1
}
worker_pids > "$qualification_root/live-tool-worker-pids.txt"
[[ -s "$qualification_root/live-tool-worker-pids.txt" ]] || {
    echo "live tool loop left no resident admitted inference worker" >&2
    exit 1
}
thread_service service:threads/list \
    '{"limit":200,"sort":"newest"}' \
    "$qualification_root/live-tool-thread-list.json"
live_tool_thread="$(unique_thread_id_for_item \
    "$qualification_root/live-tool-thread-list.json" \
    directive:qualification/live_tool_loop completed)"

HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    --project "$project_root" execute \
    graph:qualification/live_tool_follow \
    --pin-project --retain-child-results --no-stream '{}' \
    > "$qualification_root/graph-follow-started.json"
assert_json_path "$qualification_root/graph-follow-started.json" status continued
graph_parent_thread="$(execution_thread_id "$qualification_root/graph-follow-started.json")"
thread_service service:threads/get \
    "{\"thread_id\":\"$graph_parent_thread\"}" \
    "$qualification_root/graph-follow-parent-suspended.json"
graph_child_thread="$(follow_child_thread_id \
    "$qualification_root/graph-follow-parent-suspended.json")"
tail_exact_thread "$graph_child_thread" \
    "$qualification_root/graph-follow-child-tail.txt"

# Child terminal delivery creates the parent's continuation successor. Its
# identity normally exists before the child's terminal SSE reaches us; bounded
# exponential discovery handles scheduler handoff without a perpetual SQLite
# polling loop. Both actual completion waits remain pushed thread streams.
graph_successor_thread=""
for delay in 0.05 0.10 0.20 0.40 0.80 1.60 3.20; do
    thread_service service:threads/chain \
        "{\"thread_id\":\"$graph_parent_thread\"}" \
        "$qualification_root/graph-follow-chain.json"
    if graph_successor_thread="$(follow_successor_thread_id \
        "$graph_parent_thread" "$qualification_root/graph-follow-chain.json")"; then
        break
    fi
    graph_successor_thread=""
    sleep "$delay"
done
[[ -n "$graph_successor_thread" ]] || {
    echo "followed graph created no exact parent successor" >&2
    exit 1
}
tail_exact_thread "$graph_successor_thread" \
    "$qualification_root/graph-follow-successor-tail.txt"
thread_service service:threads/chain \
    "{\"thread_id\":\"$graph_parent_thread\"}" \
    "$qualification_root/graph-follow-chain-final.json"
thread_service service:threads/children \
    "{\"thread_id\":\"$graph_parent_thread\"}" \
    "$qualification_root/graph-follow-children-final.json"

stop_node
[[ -z "$(worker_pids)" ]] || {
    echo "live tool-loop worker survived the proved node stop" >&2
    exit 1
}

python3 - "$node_root" "$qualification_root" "$live_tool_thread" \
    "$graph_parent_thread" "$graph_child_thread" "$graph_successor_thread" <<'PY'
import json
from pathlib import Path
import sqlite3
import sys

node = Path(sys.argv[1])
root = Path(sys.argv[2])
direct_thread, graph_parent, graph_child, graph_successor = sys.argv[3:7]
generation = json.loads(
    (node / ".ai/state/recovery/thread-projection/generation.json").read_text(
        encoding="utf-8"
    )
)
projection_path = node / ".ai/state" / generation["projection_file"]
connection = sqlite3.connect(f"file:{projection_path}?mode=ro", uri=True)
connection.row_factory = sqlite3.Row

def thread(thread_id):
    row = connection.execute(
        "SELECT thread_id, chain_root_id, upstream_thread_id, status, "
        "base_project_snapshot_hash, result_project_snapshot_hash "
        "FROM threads WHERE thread_id=?",
        (thread_id,),
    ).fetchone()
    if row is None:
        raise SystemExit(f"projection omits thread {thread_id}")
    return dict(row)

def events(thread_id):
    rows = connection.execute(
        "SELECT event_type, payload, durability FROM events "
        "WHERE thread_id=? ORDER BY chain_seq",
        (thread_id,),
    ).fetchall()
    result = []
    for row in rows:
        payload = row["payload"]
        if isinstance(payload, bytes):
            payload = payload.decode("utf-8")
        result.append({
            "event_type": row["event_type"],
            "payload": json.loads(payload),
            "durability": row["durability"],
        })
    return result

def prove_tool_loop(thread_id, label):
    observed = events(thread_id)
    starts = [item for item in observed if item["event_type"] == "tool_call_start"]
    results = [item for item in observed if item["event_type"] == "tool_call_result"]
    expected = ["qualification_read", "qualification_mutate", "qualification_verify"]
    proposed = [
        call
        for item in observed
        if item["event_type"] == "cognition_out"
        for call in (item["payload"].get("tool_calls") or [])
    ]
    if [call.get("name") for call in proposed] != expected:
        raise SystemExit(f"{label} model proposal order differs: {proposed!r}")
    if any(call.get("arguments") != {} for call in proposed):
        raise SystemExit(f"{label} model supplied undeclared tool arguments")
    if [item["payload"].get("tool") for item in starts] != expected:
        raise SystemExit(f"{label} tool proposal order differs: {starts!r}")
    if [item["payload"].get("tool") for item in results] != expected:
        raise SystemExit(f"{label} tool settlement order differs: {results!r}")
    starts_by_operation = {
        item["payload"].get("operation_id"): item for item in starts
    }
    results_by_operation = {
        item["payload"].get("operation_id"): item for item in results
    }
    if len(starts_by_operation) != 3 or set(starts_by_operation) != set(results_by_operation):
        raise SystemExit(f"{label} has unstable or duplicate tool operation ids")
    for operation_id in starts_by_operation:
        if not isinstance(operation_id, str) or len(operation_id) != 64 or any(
            char not in "0123456789abcdef" for char in operation_id
        ):
            raise SystemExit(f"{label} has a non-canonical operation id")
    leaf_results = []
    for item in results:
        result_text = item["payload"].get("result_text")
        if not isinstance(result_text, str):
            raise SystemExit(
                f"{label} tool settlement has no exact result_text: {item!r}"
            )
        try:
            dispatch = json.loads(result_text)
        except json.JSONDecodeError as error:
            raise SystemExit(
                f"{label} tool settlement result_text is not JSON: {error}"
            ) from error
        if (
            not isinstance(dispatch, dict)
            or dispatch.get("outcome_code") != "exit:0"
            or dispatch.get("error") is not None
            or not isinstance(dispatch.get("result"), dict)
        ):
            raise SystemExit(f"{label} has a failed tool settlement: {item!r}")
        leaf_results.append(dispatch["result"])
    if [item.get("step") for item in leaf_results] != ["read", "mutate", "verify"]:
        raise SystemExit(f"{label} tool result order differs: {leaf_results!r}")
    verify = leaf_results[-1]
    if verify.get("verified") is not True:
        raise SystemExit(f"{label} has no deterministic verification settlement")
    for item in starts + results:
        if item["durability"] != "durable":
            raise SystemExit(f"{label} tool event is not durable")
    return observed

direct = thread(direct_thread)
if direct["status"] != "completed":
    raise SystemExit(f"live tool directive is not terminal: {direct!r}")
if direct["base_project_snapshot_hash"] is None:
    raise SystemExit("live tool directive has no pinned project base")
if direct["result_project_snapshot_hash"] == direct["base_project_snapshot_hash"]:
    raise SystemExit("live tool directive did not produce a private project generation")
direct_events = prove_tool_loop(direct_thread, "direct live Qwen")

parent = thread(graph_parent)
child = thread(graph_child)
successor = thread(graph_successor)
if parent["status"] != "continued":
    raise SystemExit(f"graph parent did not suspend: {parent!r}")
if child["status"] != "completed" or successor["status"] != "completed":
    raise SystemExit(
        f"graph follow did not settle child and successor: child={child!r}, successor={successor!r}"
    )
if successor["upstream_thread_id"] != graph_parent:
    raise SystemExit("graph successor does not continue the suspended parent")
child_events = prove_tool_loop(graph_child, "followed live Qwen")
parent_events = events(graph_parent)
successor_events = events(graph_successor)
if sum(item["event_type"] == "graph_follow_suspended" for item in parent_events) != 1:
    raise SystemExit("graph parent has no exact single follow suspension")
if sum(item["event_type"] == "thread_completed" for item in successor_events) != 1:
    raise SystemExit("graph successor has no exact single terminal")

evidence = {
    "direct_thread": direct,
    "direct_events": direct_events,
    "graph_parent": parent,
    "graph_parent_events": parent_events,
    "graph_child": child,
    "graph_child_events": child_events,
    "graph_successor": successor,
    "graph_successor_events": successor_events,
}
(root / "live-tool-and-graph-evidence.json").write_text(
    json.dumps(evidence, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
connection.close()
PY

# A successful static projection is observation, never a lease. In one daemon
# generation, validate the exact profile as ready, release its current binding,
# and immediately launch before any intervening validation. The launch must
# re-check live authority, return the exact typed absence, and make no thread,
# provider-attempt, or worker contact. Only then project both consumers again.
stop_node
start_node
[[ -z "$(worker_pids)" ]] || {
    echo "binding-release phase started with a resident local-inference worker" >&2
    exit 1
}
binding_id="$(tr -d '\n' < "$qualification_root/release-binding-id-4096.txt")"
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-before-release-ready-validation.json"
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" validate \
    directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
    --ref-binding model=directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
    --no-project --input '{}' \
    > "$qualification_root/validation-release-ready-qwen3-0.6b-cpu-4096.json"
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-after-release-ready-validation.json"
[[ -z "$(worker_pids)" ]] || {
    echo "release-ready validation contacted a local-inference worker" >&2
    exit 1
}
python3 - "$qualification_root" <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])
value = json.loads(
    (root / "validation-release-ready-qwen3-0.6b-cpu-4096.json").read_text()
)

def find(item):
    if isinstance(item, dict):
        if "runtime_preparation" in item and "admission_ready" in item:
            return item
        for child in item.values():
            found = find(child)
            if found is not None:
                return found
    elif isinstance(item, list):
        for child in item:
            found = find(child)
            if found is not None:
                return found
    return None

def thread_ids(path):
    value = json.loads(path.read_text())
    found = set()
    def walk(item):
        if isinstance(item, dict):
            if isinstance(item.get("thread_id"), str):
                found.add(item["thread_id"])
            for child in item.values():
                walk(child)
        elif isinstance(item, list):
            for child in item:
                walk(child)
    walk(value)
    return found

result = find(value)
dependency = next(iter(result["runtime_preparation"]["execution_dependencies"].values()))
if not result["admission_ready"] or not dependency["admission_ready"]:
    raise SystemExit("exact 4096 profile was not ready immediately before release")
if thread_ids(root / "threads-before-release-ready-validation.json") != thread_ids(
    root / "threads-after-release-ready-validation.json"
):
    raise SystemExit("release-ready validation changed thread inventory")
PY
snapshot_provider_bank "$qualification_root/bank-before-release-refusal.json" 2
thread_service service:external-content/release \
    "{\"binding_id\":\"$binding_id\"}" \
    "$qualification_root/released-binding-4096.json"
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-before-release-refusal.json"
released_refusal_raw="$qualification_root/.released-binding-launch-refusal.raw"
if NO_COLOR=1 HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
    directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
    --ref-binding model=directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
    --no-project --no-stream --input '{}' \
    > "$released_refusal_raw" 2>&1; then
    echo "execution reused validation after its exact binding was released" >&2
    exit 1
fi
[[ -z "$(worker_pids)" ]] || {
    echo "released-binding launch refusal contacted a local-inference worker" >&2
    exit 1
}
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-after-release-refusal.json"
snapshot_provider_bank "$qualification_root/bank-after-release-refusal.json" 2
python3 - "$qualification_root" "$released_refusal_raw" <<'PY'
import json
from pathlib import Path
import re
import sys

root, raw_path = map(Path, sys.argv[1:3])
raw = raw_path.read_text(encoding="utf-8")
match = re.search(r"HTTP\s+(\d+):\s*(\{[^\r\n]*\})", raw)
if match is None:
    raise SystemExit("released-binding launch returned no structured HTTP error")
status = int(match.group(1))
try:
    body = json.loads(match.group(2))
except json.JSONDecodeError as error:
    raise SystemExit(f"released-binding launch error body is not exact JSON: {error}")
expected_binding = "worker:local-inference/qwen3-0.6b-cpu-4096"
if status != 404:
    raise SystemExit(f"released-binding launch returned HTTP {status}, expected 404")
if body.get("code") != "external_content_binding_unavailable":
    raise SystemExit(f"released-binding launch returned the wrong code: {body.get('code')!r}")
if body.get("retryable") is not False or body.get("binding") != expected_binding:
    raise SystemExit("released-binding launch error contradicts exact binding authority")

def thread_ids(path):
    value = json.loads(path.read_text())
    found = set()
    def walk(item):
        if isinstance(item, dict):
            if isinstance(item.get("thread_id"), str):
                found.add(item["thread_id"])
            for child in item.values():
                walk(child)
        elif isinstance(item, list):
            for child in item:
                walk(child)
    walk(value)
    return found

if thread_ids(root / "threads-before-release-refusal.json") != thread_ids(
    root / "threads-after-release-refusal.json"
):
    raise SystemExit("released-binding launch refusal changed thread inventory")
before = json.loads((root / "bank-before-release-refusal.json").read_text())
after = json.loads((root / "bank-after-release-refusal.json").read_text())
if before != after:
    raise SystemExit("released-binding launch refusal changed provider accounting/replay state")
proof = {
    "schema": "ryeos.local_inference_binding_refusal_proof.v1",
    "http_status": status,
    "code": body["code"],
    "retryable": body["retryable"],
    "binding": body["binding"],
    "thread_inventory_unchanged": True,
    "provider_bank_unchanged": True,
    "worker_contact": False,
}
(root / "released-binding-launch-refusal.json").write_text(
    json.dumps(proof, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
raw_path.unlink()
PY
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-before-released-validation.json"
for profile in qwen3-0.6b-cpu-4096 qwen3-0.6b-cpu-2048; do
    directive="directive:local-inference/examples/${profile//[-.]/_}_smoke"
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" validate \
        "$directive" --ref-binding "model=$directive" --no-project --input '{}' \
        > "$qualification_root/validation-released-$profile.json"
done
thread_service service:threads/list '{"limit":200,"sort":"newest"}' \
    "$qualification_root/threads-after-released-validation.json"
[[ -z "$(worker_pids)" ]] || {
    echo "released-binding validation contacted a local-inference worker" >&2
    exit 1
}
python3 - "$qualification_root" <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])

def find(item):
    if isinstance(item, dict):
        if "runtime_preparation" in item and "admission_ready" in item:
            return item
        for child in item.values():
            found = find(child)
            if found is not None:
                return found
    elif isinstance(item, list):
        for child in item:
            found = find(child)
            if found is not None:
                return found
    return None

def thread_ids(path):
    value = json.loads(path.read_text())
    found = set()
    def walk(item):
        if isinstance(item, dict):
            if isinstance(item.get("thread_id"), str):
                found.add(item["thread_id"])
            for child in item.values():
                walk(child)
        elif isinstance(item, list):
            for child in item:
                walk(child)
    walk(value)
    return found

if thread_ids(root / "threads-before-released-validation.json") != thread_ids(
    root / "threads-after-released-validation.json"
):
    raise SystemExit("released-binding validation changed thread inventory")

released = find(json.loads((root / "validation-released-qwen3-0.6b-cpu-4096.json").read_text()))
retained = find(json.loads((root / "validation-released-qwen3-0.6b-cpu-2048.json").read_text()))
released_dependency = next(iter(
    released["runtime_preparation"]["execution_dependencies"].values()
))
retained_dependency = next(iter(
    retained["runtime_preparation"]["execution_dependencies"].values()
))
if released["admission_ready"] or released_dependency["admission_ready"]:
    raise SystemExit("released exact binding remained ready")
model = next(
    item for item in released_dependency["external_content"]["declarations"]
    if item["id"] == "model"
)
if model["status"] != "missing_binding" or model["binding_digest"] is not None:
    raise SystemExit(f"released exact binding projected incorrectly: {model!r}")
if not retained["admission_ready"] or not retained_dependency["admission_ready"]:
    raise SystemExit("consumer-specific release affected the other exact profile")
PY

python3 - "$qualification_root" "$evidence_output" <<'PY'
import json
import os
from pathlib import Path
import sys

root = Path(sys.argv[1])
evidence_output = Path(sys.argv[2]) if sys.argv[2] else None

def validation_result(phase, profile):
    value = json.loads((root / f"validation-{phase}-{profile}.json").read_text())
    def find(item):
        if isinstance(item, dict):
            if "runtime_preparation" in item and "admission_ready" in item:
                return item
            for child in item.values():
                found = find(child)
                if found is not None:
                    return found
        elif isinstance(item, list):
            for child in item:
                found = find(child)
                if found is not None:
                    return found
        return None
    result = find(value)
    if result is None:
        raise SystemExit(f"validation evidence omitted {phase}/{profile}")
    return result

profiles = ("qwen3-0.6b-cpu-4096", "qwen3-0.6b-cpu-2048")
resolution_identities = {}
for profile in profiles:
    identities = []
    for phase in ("before", "after", "refused", "released"):
        dependency = next(iter(
            validation_result(phase, profile)["runtime_preparation"]
            ["execution_dependencies"].values()
        ))
        identities.append(json.dumps(
            dependency["resolution"], sort_keys=True, separators=(",", ":")
        ))
    if len(set(identities)) != 1:
        raise SystemExit(f"dependency resolution identity moved across phases for {profile}")
    resolution_identities[profile] = identities[0]
if len(set(resolution_identities.values())) != len(profiles):
    raise SystemExit("the two exact profiles share a dependency resolution identity")

summary = {
    "schema": "ryeos.local_inference_node_qualification.v1",
    "external_content_policy": json.loads(
        (root / "external-policy-evidence.json").read_text()
    ),
    "activations": {
        "qwen3-0.6b-cpu-4096": {
            "first": json.loads((root / "activation-first.json").read_text()),
            "idempotent": json.loads((root / "activation-idempotent.json").read_text()),
        },
        "qwen3-0.6b-cpu-2048": {
            "first": json.loads((root / "activation-2048-first.json").read_text()),
            "idempotent": json.loads((root / "activation-2048-idempotent.json").read_text()),
        },
    },
    "validation": {
        phase: {
            profile: json.loads(
                (root / f"validation-{phase}-{profile}.json").read_text()
            )
            for profile in ("qwen3-0.6b-cpu-4096", "qwen3-0.6b-cpu-2048")
        }
        for phase in ("before", "after", "refused", "released")
    },
    "validation_thread_inventory": {
        phase: {
            side: json.loads((root / f"threads-{side}-{phase}-validation.json").read_text())
            for side in ("before", "after")
        }
        for phase in ("ready", "refusal", "released")
    } | {
        "unactivated": {
            side: json.loads((root / f"threads-{side}-validation.json").read_text())
            for side in ("before", "after")
        }
    },
    "managed_cache": json.loads((root / "managed-cache-first.json").read_text()),
    "worker_pids": {
        "qwen3-0.6b-cpu-4096": (root / "worker-pids-4096.txt").read_text().split(),
        "qwen3-0.6b-cpu-2048": (root / "worker-pids-2048.txt").read_text().split(),
    },
    "bank_before_replay": json.loads(
        (root / "bank-before-replay.json").read_text()
    ),
    "bank_after_replay": json.loads(
        (root / "bank-after-replay.json").read_text()
    ),
    "executed": {
        "qwen3-0.6b-cpu-4096": json.loads((root / "executed-4096.json").read_text()),
        "qwen3-0.6b-cpu-2048": json.loads((root / "executed-2048.json").read_text()),
    },
    "replayed": {
        "qwen3-0.6b-cpu-4096": json.loads((root / "replayed-4096.json").read_text()),
        "qwen3-0.6b-cpu-2048": json.loads((root / "replayed-2048.json").read_text()),
    },
    "worker_pids_after_replay": [],
    "live_tool_loop": json.loads((root / "live-tool-executed.json").read_text()),
    "graph_follow": json.loads((root / "live-tool-and-graph-evidence.json").read_text()),
    "released_binding": json.loads(
        (root / "released-binding-4096.json").read_text()
    ),
    "release_transition_validation": json.loads(
        (root / "validation-release-ready-qwen3-0.6b-cpu-4096.json").read_text()
    ),
    "released_binding_launch_refusal": json.loads(
        (root / "released-binding-launch-refusal.json").read_text()
    ),
}
(root / "qualification.json").write_text(
    json.dumps(summary, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
if evidence_output is not None:
    temporary = evidence_output.with_name(evidence_output.name + ".tmp")
    temporary.write_bytes((root / "qualification.json").read_bytes())
    os.replace(temporary, evidence_output)
reported_evidence = evidence_output or (root / "qualification.json")
print(json.dumps({"status": "passed", "evidence": str(reported_evidence)}))
PY
