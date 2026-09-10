#!/usr/bin/env bash
set -euo pipefail

fixture_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
tool=$fixture_root/project/.ai/tools/test/produce.yaml
wrapper=$fixture_root/project/.ai/graphs/test/recorded-producer.yaml
producer_graph=$fixture_root/project/.ai/graphs/test/two-products.yaml
portability_runner=$fixture_root/run-portability-acceptance.sh

for dependency in awk base64 cmp cp find grep mktemp python3 readlink stat; do
  if ! command -v -- "$dependency" >/dev/null 2>&1; then
    printf 'missing fixture-check dependency: %s\n' "$dependency" >&2
    exit 77
  fi
done

bash -n "$portability_runner"
grep -F -q -- 'service:identity/public_key' "$portability_runner"
if grep -F -q -- 'identity public-key' "$portability_runner"; then
  printf '%s\n' 'portability runner uses the removed native identity subcommand' >&2
  exit 65
fi
grep -F -q -- 'service:admission/submit' "$portability_runner"
grep -F -q -- 'receive-product "$origin_remote"' "$portability_runner"
grep -F -q -- '"kind":"received","acceptance_hash"' "$portability_runner"
grep -F -q -- 'phase == after-restart' "$portability_runner"

disposable=$(mktemp -d)
trap '/usr/bin/rm -rf -- "$disposable"' EXIT
/usr/bin/mkdir -p -- "$disposable/project"
cp -a -- "$fixture_root/project/." "$disposable/project/"

producer=$disposable/producer.sh
awk '
  /^        # fixture-producer-begin$/ { copying = 1 }
  copying {
    sub(/^        /, "")
    print
    if ($0 == "# fixture-producer-end") {
      exit
    }
  }
' "$tool" > "$producer"
if [[ ! -s $producer ]]; then
  printf '%s\n' 'could not extract the signed Tool producer body' >&2
  exit 1
fi

bash --noprofile --norc "$producer" "$disposable/project"

distribution=$disposable/project/products/distribution
runtime=$distribution/runtime
scratch=$disposable/project/products/build-scratch

[[ -f $runtime/bin/program ]]
[[ $(stat -c '%a' -- "$runtime/bin/program") == 755 ]]
[[ -L $runtime/current ]]
[[ $(readlink -- "$runtime/current") == bin/program ]]
[[ -d $runtime/empty-subdir ]]
[[ -z $(find "$runtime/empty-subdir" -mindepth 1 -print -quit) ]]
[[ -f $distribution/NOTICE ]]
[[ $(stat -c '%a' -- "$distribution/NOTICE") == 640 ]]
[[ -f $scratch/intermediate/state ]]
[[ $(stat -c '%a' -- "$scratch/intermediate/state") == 600 ]]
[[ -L $scratch/latest ]]
[[ $(readlink -- "$scratch/latest") == intermediate/state ]]

program_size=$(stat -c '%s' -- "$runtime/bin/program")
program_hash=$(/usr/bin/sha256sum -- "$runtime/bin/program")
program_hash=${program_hash%% *}
[[ $program_size == 9688 ]]
[[ $program_hash == 3a55c0c7914fdeccc41ac91789116675ca07befdc1c64d8c4b1b77c4b7296d77 ]]
cmp -- <(base64 -w76 -- "$runtime/bin/program") "$fixture_root/runtime-program.b64"
[[ $("$runtime/bin/program" --offline-probe) == \
  '{"schema":"test.selected_runtime_probe.v1","marker":"selected-runtime-program-v1"}' ]]
python3 - "$runtime/bin/program" <<'PY'
from pathlib import Path
import struct
import sys

binary = Path(sys.argv[1]).read_bytes()
if binary[:4] != b"\x7fELF" or binary[4:6] != b"\x02\x01":
    raise SystemExit("fixture runtime is not a little-endian ELF64 executable")
if struct.unpack_from("<H", binary, 18)[0] != 62:
    raise SystemExit("fixture runtime is not Linux x86-64 machine code")
program_offset = struct.unpack_from("<Q", binary, 32)[0]
program_size = struct.unpack_from("<H", binary, 54)[0]
program_count = struct.unpack_from("<H", binary, 56)[0]
types = [
    struct.unpack_from("<I", binary, program_offset + index * program_size)[0]
    for index in range(program_count)
]
if 3 in types:
    raise SystemExit("fixture runtime unexpectedly requires a dynamic interpreter")
PY

# Exercise only the deterministic, unsigned source-population step. The exact
# manifest file is already in RyeOS canonical JSON key order. Normalize only
# its text-file trailing newline away before hashing the exact CAS byte string.
expected_manifest=94c66e97825a25091abe924ae3de4715d26469332aa59f31cfb39854bdb59881
expected_distribution_manifest=1e3ec95d5f220c500598907f7647dd0a08dddf731fdf93da5be1bbc07b36f7b3
observed_manifest=$(python3 -c '
import hashlib, json, sys
path = sys.argv[1]
source = open(path, encoding="utf-8").read()
value = json.loads(source)
canonical = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
if source not in (canonical, canonical + "\n"):
    raise SystemExit("expected runtime manifest is not canonical JSON")
print(hashlib.sha256(canonical.encode()).hexdigest())
' "$fixture_root/expected-runtime-manifest.json")
[[ $observed_manifest == "$expected_manifest" ]]
qualification_result=$("$runtime/bin/program" --qualification-probe "$expected_manifest")
python3 - "$expected_manifest" "$qualification_result" <<'PY'
import json
import sys

value = json.loads(sys.argv[2])
if value != {
    "schema": "ryeos.product_qualification_result.v1",
    "subject_manifest_hash": sys.argv[1],
    "claims": ["runtime_program_executed"],
    "probe_evidence": {
        "schema": "test.selected_runtime_probe.v1",
        "marker": "selected-runtime-program-v1",
    },
}:
    raise SystemExit("opaque direct fixture did not return the raw qualification result")
PY
rpc_result=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"fixture/credential/start","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"fixture/credential/read","params":{}}' \
  '{"jsonrpc":"2.0","id":4,"method":"fixture/session/run","params":{}}' \
  | "$runtime/bin/program")
python3 - "$rpc_result" <<'PY'
import json
import sys

observed = [json.loads(line) for line in sys.argv[1].splitlines()]
expected = [
    {"id": 1, "result": {"ready": True}},
    {"id": 2, "result": {"login_id": "fixture-login-v1"}},
    {"id": 3, "result": {"account": {"email": "offline@example.test", "type": "fixture"}}},
    {"id": 4, "result": {
        "schema": "test.selected_runtime_execution.v1",
        "marker": "selected-runtime-program-v1",
        "network_contacted": False,
    }},
]
if observed != expected:
    raise SystemExit("fixture runtime returned a different structured-session transcript")
PY
python3 - "$fixture_root/expected-runtime-manifest.json" "$program_hash" "$program_size" <<'PY'
import json
import sys

manifest = json.load(open(sys.argv[1], encoding="utf-8"))
program = next(entry for entry in manifest["entries"] if entry.get("path") == "bin/program")
if program != {
    "blob_hash": sys.argv[2],
    "kind": "file",
    "mode": 0o755,
    "path": "bin/program",
    "size": int(sys.argv[3]),
}:
    raise SystemExit("expected manifest does not describe the produced executable")
if manifest["entry_count"] != 4 or manifest["total_bytes"] != int(sys.argv[3]):
    raise SystemExit("expected manifest metrics do not describe the produced runtime")
PY
observed_distribution_manifest=$(python3 - \
  "$distribution/NOTICE" "$program_hash" "$program_size" <<'PY'
import hashlib
import json
from pathlib import Path
import sys

notice = Path(sys.argv[1]).read_bytes()
notice_hash = hashlib.sha256(notice).hexdigest()
manifest = {
    "entries": [
        {"blob_hash": notice_hash, "kind": "file", "mode": 0o644,
         "path": "NOTICE", "size": len(notice)},
        {"kind": "dir", "path": "licenses"},
        {"kind": "dir", "path": "runtime"},
        {"kind": "dir", "path": "runtime/bin"},
        {"blob_hash": sys.argv[2], "kind": "file", "mode": 0o755,
         "path": "runtime/bin/program", "size": int(sys.argv[3])},
        {"kind": "symlink", "path": "runtime/current", "target": "bin/program"},
        {"kind": "dir", "path": "runtime/empty-subdir"},
    ],
    "entry_count": 7,
    "kind": "external_large_content_manifest",
    "schema": "ryeos.external_content.large.v2",
    "total_bytes": len(notice) + int(sys.argv[3]),
}
canonical = json.dumps(manifest, sort_keys=True, separators=(",", ":"))
print(hashlib.sha256(canonical.encode()).hexdigest())
PY
)
[[ $observed_distribution_manifest == "$expected_distribution_manifest" ]]
qualification_bundle=$disposable/qualification-bundle
"$fixture_root/prepare-qualification-bundle.sh" \
  "$observed_manifest" \
  "$qualification_bundle" >/dev/null
verifier=$qualification_bundle/standard/.ai/tools/test/verify-runtime.yaml
policy=$qualification_bundle/standard/.ai/config/test/runtime-qualification.yaml
worker=$qualification_bundle/codex/.ai/workers/fixture/hosted.yaml
enrollment_worker=$qualification_bundle/codex/.ai/workers/fixture/enrollment.yaml
login=$qualification_bundle/codex/.ai/worker-executions/fixture/login.yaml
session=$qualification_bundle/codex/.ai/worker-executions/fixture/session.yaml
profile=$qualification_bundle/codex/.ai/workers/fixture/lib/hosted/profile.json
[[ -f $verifier && -f $policy && -f $worker && -f $enrollment_worker ]]
[[ -f $login && -f $session && -f $profile ]]
[[ ! -e $qualification_bundle/standard/.ai/workers ]]
[[ ! -e $qualification_bundle/standard/.ai/worker-executions ]]
grep -F -q -- 'bin:core/ryeos-structured-session-bridge' "$worker"
grep -F -q -- 'bin:core/ryeos-structured-session-bridge' "$enrollment_worker"
[[ $(grep -F -c -- "$expected_manifest" "$verifier") -eq 2 ]]
grep -F -q -- 'mount_root: execution_runtime' "$verifier"
grep -F -q -- 'type: realization_member' "$verifier"
grep -F -q -- 'verifier_ref: tool:test/verify-runtime' "$policy"
grep -F -q -- 'subject_declaration_id: subject' "$policy"
grep -F -q -- 'allowed_claims: [runtime_program_executed]' "$policy"
grep -F -q -- 'policy_ref: config:test/runtime-qualification' \
  "$disposable/project/.ai/config/test/two-products.yaml"
grep -F -q -- 'name: distribution_to_consumer' \
  "$disposable/project/.ai/config/test/two-products.yaml"
grep -F -q -- 'policy_ref: null' \
  "$disposable/project/.ai/config/test/two-products.yaml"
grep -F -q -- "expected_manifest_hash: $expected_manifest" \
  "$disposable/project/.ai/config/test/two-products.yaml"
grep -F -q -- "expected_manifest_hash: $expected_distribution_manifest" \
  "$disposable/project/.ai/config/test/two-products.yaml"
[[ $(grep -F -c -- 'expected_manifest_hash:' \
  "$disposable/project/.ai/config/test/two-products.yaml") -eq 2 ]]
grep -F -q -- 'mount_root: execution_runtime' \
  "$disposable/project/.ai/config/test/runtime-consumer.yaml"
grep -F -q -- 'mount_root: project' \
  "$disposable/project/.ai/config/test/runtime-consumer.yaml"
grep -F -q -- 'mount: fixture-distribution' \
  "$disposable/project/.ai/config/test/runtime-consumer.yaml"
[[ $(grep -F -c -- 'relationship_ref: config:test/two-products' \
  "$disposable/project/.ai/config/test/runtime-consumer.yaml") -eq 2 ]]
grep -F -q -- 'filesystem_authority: node_policy' "$tool"
grep -F -q -- 'network_authority: isolated' "$tool"
grep -F -q -- 'filesystem_authority: captured_execution' "$verifier"

source_digest=$(sed -nE 's/^  digest: ([0-9a-f]{64})$/\1/p' "$worker")
[[ $source_digest =~ ^[0-9a-f]{64}$ ]]
[[ $(sed -nE 's/^  digest: ([0-9a-f]{64})$/\1/p' "$enrollment_worker" | head -n 1) == \
  "$source_digest" ]]
if grep -R -F -q -- 'FIXTURE_SOURCE_DIGEST' "$qualification_bundle"; then
  printf '%s\n' 'fixture Worker source digest placeholder survived preparation' >&2
  exit 1
fi
grep -F -q -- "digest: $expected_manifest" "$enrollment_worker"
grep -F -q -- 'worker_ref: worker:fixture/enrollment' "$login"
grep -F -q -- 'environment_binding: null' "$login"
grep -F -q -- 'worker_ref: null' "$session"
grep -F -q -- 'environment_binding: environment' "$session"
# This is the shared real-UID limit, not a per-worker process-pool increase.
# Match the finite production Worker ceiling in the disposable development node.
grep -F -q -- '  real_uid_process_limit: 4096' "$worker"
grep -F -q -- '  real_uid_process_limit: 4096' "$enrollment_worker"
# Session runtime inputs are not the credential-profile administration wire.
grep -F -q -- '"credential_profile_id":sys.argv[1]' "$fixture_root/run-live-acceptance.sh"
grep -F -q -- '--product-selections "$selection_json" --input "$login_parameters"' \
  "$fixture_root/run-live-acceptance.sh"
grep -F -q -- 'worker_execution:fixture/session --current-head --async --no-stream' \
  "$fixture_root/run-live-acceptance.sh"
grep -F -q -- '--continue-selected-worker-from)' \
  "$fixture_root/run-live-acceptance.sh"
grep -F -q -- 'worker-thread-project-authority' \
  "$fixture_root/run-live-acceptance.sh"
if grep -F -q -- 'worker_execution:fixture/session --pin-project' \
  "$fixture_root/run-live-acceptance.sh"; then
  printf '%s\n' 'selected Worker launch recaptures the composed consumer generation' >&2
  exit 1
fi
if grep -F -q -- 'login_parameters=$profile_request' "$fixture_root/run-live-acceptance.sh"; then
  printf '%s\n' 'session launch reuses the incompatible profile administration request' >&2
  exit 1
fi
python3 - "$profile" <<'PY'
import json
import sys

profile = json.load(open(sys.argv[1], encoding="utf-8"))
if profile["workload_realization_id"] != "runtime":
    raise SystemExit("fixture Worker does not execute the admitted runtime realization")
if profile["workload_executable"] != "bin/program":
    raise SystemExit("fixture Worker does not execute the nested admitted runtime member")
if profile["credential_subject"] != {
    "schema": 1,
    "contract": "fixture.account.v1",
    "json_pointers": ["/email", "/type"],
}:
    raise SystemExit("fixture Worker credential projection changed")
if profile["route_sets"] != {
    "enrollment": ["credential.account.read", "credential.login.start"],
    "session": ["session.run"],
}:
    raise SystemExit("fixture Worker route-set authority changed")
for route in profile["routes"]:
    # The offline workload has no independently named upstream session to
    # bind. Current structured-session profiles omit this optional field;
    # an explicit null is present-but-invalid at engine admission.
    if "session_binding" in route:
        raise SystemExit(
            f"fixture route {route.get('id')} unexpectedly declares an upstream session binding"
        )
session_route = next(route for route in profile["routes"] if route["id"] == "session.run")
literal = lambda value: {"op": "literal", "value": value}
expected_turn = "fixture-selected-runtime-turn-v1"
if session_route["observations"] != [
    {
        "when": [],
        "value": {
            "op": "object",
            "fields": {
                "kind": literal("state"),
                "expected": literal("idle"),
                "next": literal("turn_running"),
                "turn_id": literal(expected_turn),
            },
        },
    },
    {
        "when": [],
        "value": {
            "op": "object",
            "fields": {
                "kind": literal("state"),
                "expected": literal("turn_running"),
                "next": literal("idle"),
                "completed_turn_id": literal(expected_turn),
            },
        },
    },
]:
    raise SystemExit("fixture selected-runtime route lost its exact completed-turn testimony")
PY

grep -F -q -- 'service:worker-executions/command-observation' \
  "$fixture_root/run-live-acceptance.sh"
grep -F -q -- '"reason":"cancelled"' "$fixture_root/run-live-acceptance.sh"

# Contract E uses the ordinary Graph action contract: the signed wrapper owns
# one recorded inline action and delegates only through the producer capability.
grep -F -q -- 'ryeos.execute.graph.test/two-products' "$wrapper"
grep -F -q -- 'ryeos.execute.config.test/two-products' "$wrapper"
grep -F -q -- 'ryeos.execute.tool.test/produce' "$wrapper"
grep -F -q -- 'effects: recorded' "$wrapper"
grep -F -q -- 'item_id: graph:test/two-products' "$wrapper"
grep -F -q -- 'output: "${state.accepted_products}"' "$wrapper"
grep -F -q -- 'effects: recorded' "$producer_graph"
grep -F -q -- 'product_recipe: config:test/two-products' "$producer_graph"
if grep -Eq -- '(^|[[:space:]])(follow|detach|product_recipe|effect_id|controller|service):' "$wrapper"; then
  printf '%s\n' 'recorded producer wrapper contains forbidden authority or lifecycle syntax' >&2
  exit 1
fi

printf '%s\n' \
  'fixture filesystem semantics check passed' \
  'unsigned fixed-pin qualification and offline Worker Bundle population check passed' \
  'ordinary recorded producer wrapper shape check passed' \
  'RyeOS signing, Bundle installation, launch, freeze, capture, qualification, recovery, and composition were not run'
