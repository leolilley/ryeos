#!/usr/bin/env bash

# Qualify the exact local-inference release through a fresh disposable RyeOS
# node. This is deliberately separate from archive authoring: it exercises the
# installed bundle contract, fresh-cache online managed activation, the real
# daemon-owned persistent session, durable banking, restart, and zero-contact
# replay.

set -euo pipefail
export LC_ALL=C

usage() {
    echo "usage: $0 --release-contract PATH --bundle-source DIR --ryeos-bin PATH [--trust-file PATH] [--qualification-parent DIR] [--keep]" >&2
    exit 2
}

release_contract=""
bundle_source=""
ryeos_bin=""
trust_file=""
qualification_parent="${RUNNER_TEMP:-/tmp}"
keep=0
while (($#)); do
    case "$1" in
        --release-contract) release_contract="${2:-}"; shift 2 ;;
        --bundle-source) bundle_source="${2:-}"; shift 2 ;;
        --ryeos-bin) ryeos_bin="${2:-}"; shift 2 ;;
        --trust-file) trust_file="${2:-}"; shift 2 ;;
        --qualification-parent) qualification_parent="${2:-}"; shift 2 ;;
        --keep) keep=1; shift ;;
        *) usage ;;
    esac
done

[[ -f "$release_contract" && -d "$bundle_source/.ai" && -x "$ryeos_bin" ]] || usage
[[ -z "$trust_file" || -f "$trust_file" ]] || usage
[[ -d "$qualification_parent" ]] || usage

release_contract="$(realpath "$release_contract")"
bundle_source="$(realpath "$bundle_source")"
ryeos_bin="$(realpath "$ryeos_bin")"
qualification_parent="$(realpath "$qualification_parent")"
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
    contract="$(cd "$(dirname "$0")/../.." && pwd)/scripts/release/local-inference-qwen3-0.6b-v1.json"
fi
cmp "$contract" "$release_contract"

qualification_root="$(mktemp -d "$qualification_parent/ryeos-local-inference-node.XXXXXX")"
node_root="$qualification_root/node"
policy_root="$qualification_root/policy"
home_root="$qualification_root/home"
uds_path="$qualification_root/ryeosd.sock"
mkdir -p "$policy_root" "$home_root"

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
    "$policy_root/persistent-sessions.json" <<'PY'
import json
from pathlib import Path
import sys

contract = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
realizations = contract["realizations"]
bounds = [item["bounds"] for item in realizations]
external = {
    "schema": 1,
    "roots": {},
    "limits": {
        "max_depth": max(item["maximum_depth"] for item in bounds),
        "max_entries": max(item["maximum_entries"] for item in bounds),
        "max_file_bytes": max(item["maximum_file_bytes"] for item in bounds),
        "max_total_bytes": max(item["maximum_total_bytes"] for item in bounds),
        "store_budget_bytes": 4 * 1024**3,
        "minimum_free_bytes": 8 * 1024**3,
    },
    "managed_activation": {
        "allow_online": True,
        "allowed_https_hosts": [
            "github.com",
            "release-assets.githubusercontent.com",
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
        "minimum_free_bytes": 8 * 1024**3,
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

init_args=(
    init --non-interactive --app-root "$node_root" --source "$bundle_source"
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
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" \
        PATH="$(dirname "$ryeos_bin"):$PATH" \
        "$ryeos_bin" start --app-root "$node_root" \
        --bind 127.0.0.1:0 --uds-path "$uds_path" >/dev/null
    node_started=1
}

stop_node() {
    HOME="$home_root" RYEOS_APP_ROOT="$node_root" \
        PATH="$(dirname "$ryeos_bin"):$PATH" \
        "$ryeos_bin" stop --app-root "$node_root" >/dev/null
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
    (node / ".ai/state/generation.json").read_text(encoding="utf-8")
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
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    external-content activate \
    config:ryeos-runtime/local-tinygrad-activation online \
    > "$qualification_root/activation-first.json"
verify_managed_cache "$qualification_root/managed-cache-first.json"
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" \
    external-content activate \
    config:ryeos-runtime/local-tinygrad-activation online \
    > "$qualification_root/activation-idempotent.json"
assert_json_path "$qualification_root/activation-first.json" idempotent false
assert_json_path "$qualification_root/activation-idempotent.json" idempotent true
verify_managed_cache "$qualification_root/managed-cache-idempotent.json"
cmp "$qualification_root/managed-cache-first.json" \
    "$qualification_root/managed-cache-idempotent.json"

HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" validate \
    directive:local-inference/examples/tinygrad_smoke \
    --ref-binding model=directive:local-inference/examples/tinygrad_smoke \
    '{}' > "$qualification_root/validation.json"
HOME="$home_root" RYEOS_APP_ROOT="$node_root" "$ryeos_bin" execute \
    directive:local-inference/examples/tinygrad_smoke \
    --ref-binding model=directive:local-inference/examples/tinygrad_smoke \
    --no-stream '{}' > "$qualification_root/executed.json"
assert_json_path "$qualification_root/executed.json" result.outcome_code success
assert_json_path "$qualification_root/executed.json" result.result OK
assert_json_path "$qualification_root/executed.json" result.error null
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
    --no-stream '{}' > "$qualification_root/replayed.json"
assert_json_path "$qualification_root/replayed.json" result.outcome_code success
assert_json_path "$qualification_root/replayed.json" result.result OK
assert_json_path "$qualification_root/replayed.json" result.error null
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

python3 - "$qualification_root" <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])
summary = {
    "schema": "ryeos.local_inference_node_qualification.v1",
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
}
(root / "qualification.json").write_text(
    json.dumps(summary, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
print(json.dumps({"status": "passed", "evidence": str(root / "qualification.json")}))
PY
