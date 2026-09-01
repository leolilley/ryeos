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
    echo "usage: $0 --release-contract PATH --bundle-source DIR --ryeos-bin PATH (--online | --archive-root DIR) [--minimum-free-bytes BYTES] [--trust-file PATH] [--qualification-parent DIR] [--keep]" >&2
    exit 2
}

release_contract=""
bundle_source=""
ryeos_bin=""
trust_file=""
qualification_parent="${RUNNER_TEMP:-/tmp}"
online=0
archive_root=""
minimum_free_bytes="8589934592"
keep=0
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
else
    [[ -n "$archive_root" && -d "$archive_root" ]] || usage
fi

release_contract="$(realpath "$release_contract")"
bundle_source="$(realpath "$bundle_source")"
ryeos_bin="$(realpath "$ryeos_bin")"
qualification_parent="$(realpath "$qualification_parent")"
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
}
persistent = {
    "schema": 1,
    "limits": {
        "max_pool_groups": 4,
        "max_total_processes": 1,
        "max_total_address_space_bytes": 16 * 1024**3,
        "max_total_cpu_seconds": 3600,
        "max_open_streams": 8,
        "max_active_streams": 1,
        "max_active_streams_per_subject": 1,
        "max_stream_backlog_bytes": 16 * 1024**2,
        "max_total_backlog_bytes": 16 * 1024**2,
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
  provider: local-tinygrad
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
)
if [[ -n "$trust_file" ]]; then
    init_args+=(--trust-file "$trust_file")
fi
HOME="$home_root" "$ryeos_bin" "${init_args[@]}" >/dev/null
HOME="$home_root" "$ryeos_bin" node policy-apply external_content \
    "$policy_root/external-content.json" --app-root "$node_root" --json \
    > "$qualification_root/external-content-policy-result.json"
HOME="$home_root" "$ryeos_bin" node policy-apply persistent_sessions \
    "$policy_root/persistent-sessions.json" --app-root "$node_root" --json \
    > "$qualification_root/persistent-sessions-policy-result.json"

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
    python3 - "$node_root" "$evidence_file" <<'PY'
import json
from pathlib import Path
import sqlite3
import sys

node = Path(sys.argv[1])
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
if len(attempts) != 1:
    raise SystemExit(f"expected one provider attempt, observed {len(attempts)}")
attempt = attempts[0]
expected_attempt = {
    "state": "reconciled",
    "reserved_usd_nanos": 0,
    "budget_charge_usd_nanos": 0,
    "provider_actual_usd_nanos": 0,
    "charge_basis": "explicitly_free",
    "reconciliation_reason": "explicitly_free_contract",
}
for field, expected in expected_attempt.items():
    if attempt[field] != expected:
        raise SystemExit(
            f"provider attempt {field}: expected={expected!r}, observed={attempt[field]!r}"
        )
if attempt["settled_at_ms"] is None:
    raise SystemExit("provider attempt is terminal without settled_at_ms")
publication = [
    item for item in operations
    if item["operation_kind"] == "provider_call_publication"
    and item["transition_sequence"] == 1
]
if len(publication) != 1:
    raise SystemExit(
        f"expected one provider_call_publication operation, observed {len(publication)}"
    )
if len(records) != 1:
    raise SystemExit(f"expected one provider replay record, observed {len(records)}")
proof = json.loads(publication[0]["response_json"])
record = records[0]
for field in ("cache_key", "answer_digest", "record_hash"):
    if proof.get(field) != record[field]:
        raise SystemExit(
            f"provider publication proof {field} contradicts replay record"
        )
evidence = {
    "attempts": attempts,
    "operations": operations,
    "provider_record": record,
    "provider_observations": observations,
}
Path(sys.argv[2]).write_text(
    json.dumps(evidence, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY
}

start_node
activation_args=(
    external-content activate
    config:ryeos-runtime/local-tinygrad-activation
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

HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    --project "$project_root" sign \
    tool:qualification/read \
    tool:qualification/mutate \
    tool:qualification/verify \
    config:ryeos-runtime/execution \
    directive:qualification/live_tool_loop \
    graph:qualification/live_tool_follow \
    > "$qualification_root/project-signing.json"

HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" validate \
    directive:local-inference/examples/tinygrad_smoke \
    --ref-binding model=directive:local-inference/examples/tinygrad_smoke \
    --no-project --input '{}' > "$qualification_root/validation.json"
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
    directive:local-inference/examples/tinygrad_smoke \
    --ref-binding model=directive:local-inference/examples/tinygrad_smoke \
    --no-project --no-stream --input '{}' > "$qualification_root/executed.json"
assert_json_path "$qualification_root/executed.json" status completed
assert_json_path "$qualification_root/executed.json" success true
assert_json_path "$qualification_root/executed.json" result OK
assert_json_missing "$qualification_root/executed.json" error
worker_pids > "$qualification_root/first-worker-pids.txt"
[[ -s "$qualification_root/first-worker-pids.txt" ]] || {
    echo "first local-inference execution left no resident admitted worker" >&2
    exit 1
}

stop_node
[[ -z "$(worker_pids)" ]] || {
    echo "local-inference worker survived the proved node stop" >&2
    exit 1
}
snapshot_provider_bank "$qualification_root/bank-before-replay.json"
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
python3 - "$qualification_root/bank-before-replay.json" <<'PY'
import datetime
import json
from pathlib import Path
import sys
import time

bank = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
recorded = datetime.datetime.fromisoformat(
    bank["provider_record"]["last_replayed_at"].replace("Z", "+00:00")
)
target = recorded + datetime.timedelta(seconds=1)
deadline = time.monotonic() + 2.0
while datetime.datetime.now(datetime.timezone.utc) < target:
    if time.monotonic() >= deadline:
        raise SystemExit("clock did not advance beyond replay timestamp precision")
    time.sleep(0.05)
PY
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
    directive:local-inference/examples/tinygrad_smoke \
    --ref-binding model=directive:local-inference/examples/tinygrad_smoke \
    --no-project --no-stream --input '{}' > "$qualification_root/replayed.json"
assert_json_path "$qualification_root/replayed.json" status completed
assert_json_path "$qualification_root/replayed.json" success true
assert_json_path "$qualification_root/replayed.json" result OK
assert_json_missing "$qualification_root/replayed.json" error
[[ -z "$(worker_pids)" ]] || {
    echo "provider replay spawned or contacted a local-inference worker" >&2
    exit 1
}
stop_node
snapshot_provider_bank "$qualification_root/bank-after-replay.json"

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
before_record = before["provider_record"]
after_record = after["provider_record"]
for field in ("cache_key", "answer_digest", "record_hash", "produced_at"):
    if before_record[field] != after_record[field]:
        raise SystemExit(f"provider replay changed immutable replay field {field}")
if after_record["last_replayed_at"] <= before_record["last_replayed_at"]:
    raise SystemExit("provider replay did not advance replay retention evidence")

first_observations = before["provider_observations"]
final_observations = after["provider_observations"]
if len(first_observations) != 1 or len(final_observations) != 2:
    raise SystemExit(
        "expected exactly one executed and one replayed provider observation"
    )
if final_observations[0] != first_observations[0]:
    raise SystemExit("provider replay changed the first terminal observation")
executed = first_observations[0]
replayed = final_observations[1]
for item in (executed, replayed):
    if item["durability"] != "durable" or item["thread_status"] != "completed":
        raise SystemExit("provider observation is not attached to a completed durable thread")
    if item["payload"]["record_hash"] != before_record["record_hash"]:
        raise SystemExit("provider observation contradicts the banked record hash")
    if item["payload"]["answer_digest"] != before_record["answer_digest"]:
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
if replay_source.get("attempt_id") != before["attempts"][0]["attempt_id"]:
    raise SystemExit("replayed provider observation names the wrong source attempt")
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
policy["limits"]["max_total_address_space_bytes"] = 16 * 1024**3
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

python3 - "$qualification_root" "$minimum_free_bytes" <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])
summary = {
    "schema": "ryeos.local_inference_node_qualification.v1",
    "minimum_free_bytes": int(sys.argv[2]),
    "activation": json.loads((root / "activation-first.json").read_text()),
    "idempotent_activation": json.loads(
        (root / "activation-idempotent.json").read_text()
    ),
    "managed_cache": json.loads((root / "managed-cache-first.json").read_text()),
    "first_worker_pids": (root / "first-worker-pids.txt").read_text().split(),
    "bank_before_replay": json.loads(
        (root / "bank-before-replay.json").read_text()
    ),
    "bank_after_replay": json.loads(
        (root / "bank-after-replay.json").read_text()
    ),
    "executed": json.loads((root / "executed.json").read_text()),
    "replayed": json.loads((root / "replayed.json").read_text()),
    "worker_pids_after_replay": [],
    "live_tool_loop": json.loads((root / "live-tool-executed.json").read_text()),
    "graph_follow": json.loads((root / "live-tool-and-graph-evidence.json").read_text()),
}
(root / "qualification.json").write_text(
    json.dumps(summary, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
print(json.dumps({"status": "passed", "evidence": str(root / "qualification.json")}))
PY
