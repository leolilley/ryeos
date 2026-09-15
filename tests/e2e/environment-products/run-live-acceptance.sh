#!/usr/bin/env bash
# Run only against an already initialized, populated and running disposable
# node. This script never starts/stops a node, signs source, installs a Bundle,
# discovers a project generation, or selects a global "latest" object.
set -euo pipefail

fixture_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
assert_response=$fixture_root/assert-live-response.py
expected_manifest=94c66e97825a25091abe924ae3de4715d26469332aa59f31cfb39854bdb59881
expected_distribution_manifest=1e3ec95d5f220c500598907f7647dd0a08dddf731fdf93da5be1bbc07b36f7b3
command_timeout=120
ryeos_bin=
app_root=
daemon_url=
project_root=
consumer_snapshot=
credential_profile=
evidence_dir=
continue_selected_worker_from=

usage() {
  printf '%s\n' \
    "usage: $0 --ryeos-bin <absolute-path> --app-root <absolute-path> \\" \
    '  --daemon-url <url> --project <absolute-path> \' \
    '  --consumer-project-snapshot-hash <sha256> \' \
    '  --credential-profile-id <fixture-id> --evidence-dir <new-directory>' \
    '  [--command-timeout <seconds>] \'
    '  [--continue-selected-worker-from <absolute-prior-evidence-directory>]'
}

while (($#)); do
  case $1 in
    --ryeos-bin) ryeos_bin=${2-}; shift 2 ;;
    --app-root) app_root=${2-}; shift 2 ;;
    --daemon-url) daemon_url=${2-}; shift 2 ;;
    --project) project_root=${2-}; shift 2 ;;
    --consumer-project-snapshot-hash) consumer_snapshot=${2-}; shift 2 ;;
    --credential-profile-id) credential_profile=${2-}; shift 2 ;;
    --evidence-dir) evidence_dir=${2-}; shift 2 ;;
    --command-timeout) command_timeout=${2-}; shift 2 ;;
    --continue-selected-worker-from) continue_selected_worker_from=${2-}; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 64 ;;
  esac
done
if [[ -n $continue_selected_worker_from ]]; then
  [[ $continue_selected_worker_from == /* ]] || {
    printf 'continuation evidence coordinate must be absolute: %s\n' \
      "$continue_selected_worker_from" >&2
    exit 64
  }
  [[ -d $continue_selected_worker_from ]] || {
    printf 'continuation evidence directory does not exist: %s\n' \
      "$continue_selected_worker_from" >&2
    exit 66
  }
  [[ $continue_selected_worker_from != "$evidence_dir" ]] || {
    printf '%s\n' 'continuation evidence and new evidence directories must differ' >&2
    exit 64
  }
fi

for value in "$ryeos_bin" "$app_root" "$project_root" "$evidence_dir"; do
  [[ $value == /* ]] || {
    printf 'all filesystem coordinates must be absolute: %s\n' "$value" >&2
    exit 64
  }
done
[[ -x $ryeos_bin ]] || { printf 'ryeos binary is not executable: %s\n' "$ryeos_bin" >&2; exit 66; }
[[ -d $app_root ]] || { printf 'app root does not exist: %s\n' "$app_root" >&2; exit 66; }
[[ -d $project_root ]] || { printf 'project root does not exist: %s\n' "$project_root" >&2; exit 66; }
[[ $daemon_url =~ ^https?://[^[:space:]]+$ ]] || { printf '%s\n' 'daemon URL must be explicit HTTP(S)' >&2; exit 64; }
[[ $consumer_snapshot =~ ^[0-9a-f]{64}$ ]] || { printf '%s\n' 'consumer snapshot must be a lowercase SHA-256 digest' >&2; exit 64; }
[[ $credential_profile =~ ^[a-z][a-z0-9._-]{0,127}$ ]] || { printf '%s\n' 'credential profile id must be explicit canonical fixture text' >&2; exit 64; }
[[ $command_timeout =~ ^[1-9][0-9]*$ ]] || { printf '%s\n' 'command timeout must be positive seconds' >&2; exit 64; }
[[ ! -e $evidence_dir ]] || { printf 'evidence directory already exists: %s\n' "$evidence_dir" >&2; exit 73; }

for dependency in python3 timeout; do
  command -v -- "$dependency" >/dev/null 2>&1 || {
    printf 'missing live acceptance dependency: %s\n' "$dependency" >&2
    exit 77
  }
done

/usr/bin/mkdir -p -- "$evidence_dir"
export RYEOS_APP_ROOT=$app_root
export RYEOSD_URL=$daemon_url

run_json() {
  local destination=$1
  shift
  if timeout "$command_timeout" "$ryeos_bin" "$@" >"$destination"; then
    :
  else
    local status=$?
    printf 'command failed (%s), response retained at %s: ryeos' "$status" "$destination" >&2
    printf ' %q' "$@" >&2
    printf '\n' >&2
    return "$status"
  fi
  python3 -c 'import json,sys; json.load(open(sys.argv[1], encoding="utf-8"))' "$destination"
}

# A worker launch can return before its structured-session process is attached.
# Retry one exact idempotent command coordinate; never inspect a global session
# list or substitute another root.
run_json_retry() {
  local destination=$1
  shift
  local attempt=0
  local deadline=$((SECONDS + command_timeout))
  while true; do
    attempt=$((attempt + 1))
    local attempt_file=$destination.attempt-$(printf '%04d' "$attempt").json
    local error_file=$destination.attempt-$(printf '%04d' "$attempt").stderr
    local status=0
    timeout 10 "$ryeos_bin" "$@" >"$attempt_file" 2>"$error_file" || status=$?
    if ((status == 0)) \
      && python3 -c 'import json,sys; json.load(open(sys.argv[1], encoding="utf-8"))' "$attempt_file"
    then
      /usr/bin/cp -- "$attempt_file" "$destination"
      return 0
    fi
    local disposition
    disposition=$(python3 "$assert_response" retry-disposition \
      "$attempt_file" "$error_file" "$status")
    if [[ $disposition != retry ]]; then
      printf 'exact worker command failed permanently; response retained at %s\n' \
        "$attempt_file" >&2
      /usr/bin/cat -- "$error_file" >&2 || true
      return 1
    fi
    if ((SECONDS >= deadline)); then
      printf 'exact worker command did not become ready; last response retained at %s\n' \
        "$attempt_file" >&2
      /usr/bin/cat -- "$error_file" >&2 || true
      return 1
    fi
    sleep 1
  done
}

# Bind the attached session capsule to one exact accepted launch. Thread detail
# proves the launch capsule; the root-scoped hosted-session projection supplies
# the distinct persistent-session capsule used by command testimony.
worker_session_capsule() {
  local evidence_prefix=$1
  local thread_id=$2
  local chain_root_id=$3
  local placement_thread_id=$4
  run_json "$evidence_prefix-thread.json" thread get --thread-id "$thread_id"
  python3 "$assert_response" worker-thread-authority \
    "$evidence_prefix-thread.json" "$thread_id" "$chain_root_id" >/dev/null
  local status_request
  status_request=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1]},separators=(",",":")))' \
    "$chain_root_id")
  run_json "$evidence_prefix-session.json" --no-project execute \
    service:worker-executions/status --no-stream --input "$status_request"
  python3 "$assert_response" worker-session-authority \
    "$evidence_prefix-session.json" "$chain_root_id" "$placement_thread_id"
}

# Buffered execute returns the result, not a durable thread coordinate. Accept
# once, retain its root-bearing response, then read only that exact thread.
run_execution() {
  local destination=$1
  shift
  run_json "$destination.launch.json" "$@" --async
  local coordinates
  coordinates=$(python3 "$assert_response" launch "$destination.launch.json")
  local exact
  mapfile -t exact <<<"$coordinates"
  local deadline=$((SECONDS + command_timeout))
  local attempt=0
  while true; do
    attempt=$((attempt + 1))
    local observation=$destination.observation-$(printf '%04d' "$attempt").json
    run_json "$observation" thread get --thread-id "${exact[0]}"
    local disposition
    disposition=$(python3 "$assert_response" terminal-disposition "$observation" "${exact[0]}" "${exact[1]}")
    if [[ $disposition == completed ]]; then
      /usr/bin/cp -- "$observation" "$destination"
      return 0
    fi
    if ((SECONDS >= deadline)); then
      printf 'accepted execution remains pending; do not relaunch: %s\n' "$destination.launch.json" >&2
      return 1
    fi
    sleep 1
  done
}

run_selected_worker_acceptance() {
  local selection_json login_parameters session_json session_coordinate_text
  local session_request session_command_coordinate_text selected_session_capsule
  local session_observation_request session_fence session_termination
  local session_status_request candidate_snapshot discard_request

  selection_json=$(python3 -c 'import json,sys; source={"kind":"local_capture"}; print(json.dumps([{"target":{"kind":"content_dependency","binding":"environment"},"selection":{"declaration_id":"distribution","witness_hash":sys.argv[1],"witness_source":source,"qualification_hash":None}},{"target":{"kind":"content_dependency","binding":"environment"},"selection":{"declaration_id":"runtime","witness_hash":sys.argv[2],"witness_source":source,"qualification_hash":sys.argv[3]}}],separators=(",",":")))' \
    "$distribution_witness" "$runtime_witness" "$qualification_hash")
  login_parameters=$(python3 -c 'import json,sys; print(json.dumps({"credential_profile_id":sys.argv[1]},separators=(",",":")))' \
    "$credential_profile")
  session_json=$evidence_dir/20-selected-worker-launch.json
  run_json "$session_json" --project "$project_root" execute \
    worker_execution:fixture/session --current-head --async --no-stream \
    --ref-binding environment=config:test/runtime-consumer \
    --product-selections "$selection_json" --input "$login_parameters"
  session_coordinate_text=$(python3 "$assert_response" launch "$session_json")
  mapfile -t session_coordinate <<<"$session_coordinate_text"
  session_thread=${session_coordinate[0]}
  session_root=${session_coordinate[1]}

  # The selected bindings were composed for one exact current-head generation.
  # Prove the newly accepted placement retained that generation before sending
  # any hosted command.
  run_json "$evidence_dir/20-selected-worker-thread.json" \
    thread get --thread-id "$session_thread"
  python3 "$assert_response" worker-thread-project-authority \
    "$evidence_dir/20-selected-worker-thread.json" "$session_thread" \
    "$session_root" "$consumer_snapshot"

  session_request=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1],"idempotency_key":"fixture-selected-runtime-v1","route_id":"session.run","payload":{}},separators=(",",":")))' \
    "$session_root")
  run_json_retry "$evidence_dir/21-selected-runtime-command.json" --no-project execute \
    service:worker-executions/command --no-stream --input "$session_request"
  session_command_coordinate_text=$(python3 "$assert_response" worker-result \
    "$evidence_dir/21-selected-runtime-command.json" "$session_root" "$session_thread" 1)
  mapfile -t session_command_coordinate <<<"$session_command_coordinate_text"
  selected_session_capsule=$(worker_session_capsule \
    "$evidence_dir/21-selected-runtime-authority" "$session_thread" "$session_root" \
    "${session_command_coordinate[1]}")
  session_observation_request=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1],"placement_thread_id":sys.argv[2],"command_sequence":int(sys.argv[3])},separators=(",",":")))' \
    "${session_command_coordinate[0]}" "${session_command_coordinate[1]}" \
    "${session_command_coordinate[2]}")
  run_json "$evidence_dir/21-selected-runtime-command-observation.json" --no-project execute \
    service:worker-executions/command-observation --no-stream \
    --input "$session_observation_request"
  session_fence=$(python3 "$assert_response" command-observation \
    "$evidence_dir/21-selected-runtime-command-observation.json" \
    "${session_command_coordinate[0]}" "${session_command_coordinate[1]}" \
    "${session_command_coordinate[2]}" fixture-selected-runtime-v1 \
    "${session_command_coordinate[3]}" "${session_command_coordinate[4]}" \
    "$selected_session_capsule" session.run completed-turn)
  session_termination=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1],"reason":"completed","completion":json.loads(sys.argv[2])},separators=(",",":")))' \
    "$session_root" "$session_fence")
  run_json "$evidence_dir/22-selected-worker-termination.json" --no-project execute \
    service:worker-executions/terminate --no-stream --input "$session_termination"
  python3 "$assert_response" retained-termination \
    "$evidence_dir/22-selected-worker-termination.json" \
    "$session_root"
  session_status_request=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1]},separators=(",",":")))' \
    "$session_root")
  run_json "$evidence_dir/23-selected-worker-status.json" --no-project execute \
    service:worker-executions/status --no-stream --input "$session_status_request"
  candidate_snapshot=$(python3 "$assert_response" retained-candidate \
    "$evidence_dir/23-selected-worker-status.json" "$session_root")
  discard_request=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1],"candidate_snapshot_hash":sys.argv[2]},separators=(",",":")))' \
    "$session_root" "$candidate_snapshot")
  run_json "$evidence_dir/24-selected-worker-discard.json" --no-project execute \
    service:worker-executions/discard --no-stream --input "$discard_request"
  python3 "$assert_response" discarded "$evidence_dir/24-selected-worker-discard.json" \
    "$session_root"
}

if [[ -n $continue_selected_worker_from ]]; then
  composition_source=$continue_selected_worker_from/10-composition.json
  profile_source=$continue_selected_worker_from/19-credential-profile-confirm.json
  [[ -f $composition_source && -f $profile_source ]] || {
    printf '%s\n' 'continuation requires exact steps 10 and 19 from a completed prefix' >&2
    exit 66
  }
  resume_coordinates=$(python3 "$assert_response" resume-composition \
    "$composition_source" "$consumer_snapshot" \
    "$expected_distribution_manifest" "$expected_manifest")
  mapfile -t resume_coordinate <<<"$resume_coordinates"
  # The first two values are the independently revalidated binding hashes.
  distribution_consumer_binding=${resume_coordinate[0]}
  runtime_consumer_binding=${resume_coordinate[1]}
  distribution_witness=${resume_coordinate[2]}
  runtime_witness=${resume_coordinate[3]}
  qualification_hash=${resume_coordinate[4]}

  confirmed_profile=$(python3 "$assert_response" confirmed-profile \
    "$profile_source" "$credential_profile")
  mapfile -t confirmed_profile_coordinate <<<"$confirmed_profile"
  profile_request=$(python3 -c 'import json,sys; print(json.dumps({"profile_id":sys.argv[1]},separators=(",",":")))' \
    "$credential_profile")
  run_json "$evidence_dir/19a-current-credential-profile.json" --no-project execute \
    service:credential-profiles/get --no-stream --input "$profile_request"
  python3 "$assert_response" active-profile \
    "$evidence_dir/19a-current-credential-profile.json" "$credential_profile" \
    "${confirmed_profile_coordinate[0]}" "${confirmed_profile_coordinate[1]}"

  run_selected_worker_acceptance
  printf '%s\n' \
    'qualified selected-runtime worker continuation passed' \
    "consumer_project_snapshot_hash=$consumer_snapshot" \
    "distribution_witness_hash=$distribution_witness" \
    "runtime_witness_hash=$runtime_witness" \
    "qualification_hash=$qualification_hash" \
    "credential_profile_id=$credential_profile" \
    "selected_worker_thread_id=$session_thread" \
    "selected_worker_chain_root_id=$session_root" \
    "evidence_directory=$evidence_dir"
  exit 0
fi

# The signed wrapper has no output partition. Its one ordinary recorded action
# targets the admitted producer Graph, whose awaited private root captures and
# returns the immutable accepted-product object.
wrapper_json=$evidence_dir/01-recorded-wrapper.json
run_execution "$wrapper_json" --project "$project_root" execute \
  graph:test/recorded-producer --current-head --no-stream --input '{}'
wrapper_coordinate_text=$(python3 "$assert_response" execution "$wrapper_json")
mapfile -t wrapper_coordinate <<<"$wrapper_coordinate_text"
wrapper_thread=${wrapper_coordinate[0]}
wrapper_root=${wrapper_coordinate[1]}
[[ -n $wrapper_thread && -n $wrapper_root ]]
run_json "$evidence_dir/02-recorded-wrapper-thread.json" \
  thread get --thread-id "$wrapper_thread"
python3 "$assert_response" thread "$evidence_dir/02-recorded-wrapper-thread.json" \
  "$wrapper_thread" "$wrapper_root"
accepted_witnesses=$(python3 "$assert_response" accepted "$wrapper_json" "$consumer_snapshot")
mapfile -t accepted_witness <<<"$accepted_witnesses"
distribution_witness=${accepted_witness[0]}
runtime_witness=${accepted_witness[1]}
first_receipts=$evidence_dir/02a-recorded-wrapper-receipts.json
run_json "$first_receipts" --no-project execute service:threads/receipts \
  --no-stream --input "{\"thread_id\":\"$wrapper_thread\"}"
first_effect_record=$(python3 "$assert_response" recorded-receipt \
  "$first_receipts" executed)

# An equivalent second action must return the same accepted immutable witness.
# It creates a new wrapper execution root but must not substitute another build
# answer into the recorded action coordinate.
replay_json=$evidence_dir/03-recorded-wrapper-replay.json
run_execution "$replay_json" --project "$project_root" execute \
  graph:test/recorded-producer --current-head --no-stream --input '{}'
replay_coordinate_text=$(python3 "$assert_response" execution "$replay_json")
mapfile -t replay_coordinate <<<"$replay_coordinate_text"
replay_thread=${replay_coordinate[0]}
replay_root=${replay_coordinate[1]}
run_json "$evidence_dir/04-recorded-wrapper-replay-thread.json" \
  thread get --thread-id "$replay_thread"
python3 "$assert_response" thread "$evidence_dir/04-recorded-wrapper-replay-thread.json" \
  "$replay_thread" "$replay_root"
replayed_witnesses=$(python3 "$assert_response" accepted "$replay_json" "$consumer_snapshot")
python3 "$assert_response" accepted-equal "$replay_json" "$wrapper_json"
mapfile -t replayed_witness <<<"$replayed_witnesses"
[[ ${replayed_witness[0]} == "$distribution_witness" \
  && ${replayed_witness[1]} == "$runtime_witness" ]] || {
  printf '%s\n' 'equivalent recorded action returned a different product witness batch' >&2
  exit 1
}
replay_receipts=$evidence_dir/04a-recorded-wrapper-replay-receipts.json
run_json "$replay_receipts" --no-project execute service:threads/receipts \
  --no-stream --input "{\"thread_id\":\"$replay_thread\"}"
python3 "$assert_response" recorded-receipt "$replay_receipts" \
  effect_record "$first_effect_record"
replay_chain=$evidence_dir/04b-recorded-wrapper-replay-chain.json
run_json "$replay_chain" --no-project execute service:threads/chain \
  --no-stream --input "{\"thread_id\":\"$replay_root\"}"
python3 "$assert_response" replay-chain-leaf "$replay_chain" \
  "$replay_thread" "$replay_root"

# A witness is not a consumer grant. Create a fresh one-use ordinary stage for
# the independently installed fixed-pin Bundle verifier.
run_json "$evidence_dir/05-runtime-import.json" external-content \
  import-product "$runtime_witness" '{"kind":"local_capture"}' 16384
import_coordinate_text=$(python3 "$assert_response" import \
  "$evidence_dir/05-runtime-import.json" "$expected_manifest")
mapfile -t import_coordinate <<<"$import_coordinate_text"
staging_id=${import_coordinate[0]}
request_digest=${import_coordinate[1]}
run_json "$evidence_dir/06-verifier-binding.json" --no-project external-content bind \
  "$staging_id" "$request_digest" "$expected_manifest" \
  tool:test/verify-runtime installed_bundle
python3 "$assert_response" bind "$evidence_dir/06-verifier-binding.json" "$expected_manifest"

# Execute and inspect only the exact returned verifier thread. The qualification
# request carries coordinates, not claims, policy, a manifest, or a host path.
verifier_json=$evidence_dir/07-verifier.json
run_execution "$verifier_json" --no-project execute tool:test/verify-runtime --no-stream --input '{}'
verifier_coordinate_text=$(python3 "$assert_response" execution "$verifier_json")
mapfile -t verifier_coordinate <<<"$verifier_coordinate_text"
verifier_thread=${verifier_coordinate[0]}
verifier_root=${verifier_coordinate[1]}
run_json "$evidence_dir/08-verifier-thread.json" \
  thread get --thread-id "$verifier_thread"
python3 "$assert_response" thread "$evidence_dir/08-verifier-thread.json" \
  "$verifier_thread" "$verifier_root"
python3 "$assert_response" verifier-result "$verifier_json" "$expected_manifest"

run_json "$evidence_dir/09-qualification.json" external-content qualify-product \
  "$runtime_witness" '{"kind":"local_capture"}' runtime_to_consumer \
  "$verifier_root" "$verifier_thread"
qualification_coordinate_text=$(python3 "$assert_response" qualification \
  "$evidence_dir/09-qualification.json")
mapfile -t qualification_coordinate <<<"$qualification_coordinate_text"
qualification_hash=${qualification_coordinate[0]}
qualification_coordinate_id=${qualification_coordinate[1]}

# Compose the complete selection batch against the caller-supplied exact pinned
# consumer generation. The command has no ambient project contract: its closed
# request carries the snapshot authority explicitly and no filesystem path.
compose_request=$(python3 -c 'import json,sys; source={"kind":"local_capture"}; print(json.dumps({"consumer_ref":"config:test/runtime-consumer","project_context":{"snapshot_hash":sys.argv[1]},"selections":[{"declaration_id":"distribution","witness_hash":sys.argv[2],"witness_source":source,"qualification_hash":None},{"declaration_id":"runtime","witness_hash":sys.argv[3],"witness_source":source,"qualification_hash":sys.argv[4]}],"maximum_bytes":32768},separators=(",",":")))' \
  "$consumer_snapshot" "$distribution_witness" "$runtime_witness" "$qualification_hash")
run_json "$evidence_dir/10-composition.json" \
  external-content compose-product "$compose_request"
consumer_bindings=$(python3 "$assert_response" composition \
  "$evidence_dir/10-composition.json" "$consumer_snapshot" \
  "$distribution_witness" "$runtime_witness" "$qualification_hash" \
  "$expected_distribution_manifest" "$expected_manifest")
mapfile -t consumer_binding <<<"$consumer_bindings"
distribution_consumer_binding=${consumer_binding[0]}
runtime_consumer_binding=${consumer_binding[1]}

# The enrollment Worker and the selected session Worker are intentionally
# distinct consumers. Enrollment uses a separately staged literal pin so an
# unauthenticated profile can be observed without weakening the selected
# environment's signed `active` credential requirement.
run_json "$evidence_dir/11-enrollment-runtime-import.json" \
  external-content import-product "$runtime_witness" '{"kind":"local_capture"}' 16384
enrollment_import_text=$(python3 "$assert_response" import \
  "$evidence_dir/11-enrollment-runtime-import.json" "$expected_manifest")
mapfile -t enrollment_import <<<"$enrollment_import_text"
enrollment_staging_id=${enrollment_import[0]}
enrollment_request_digest=${enrollment_import[1]}
run_json "$evidence_dir/12-enrollment-worker-binding.json" --no-project \
  external-content bind "$enrollment_staging_id" "$enrollment_request_digest" \
  "$expected_manifest" worker:fixture/enrollment installed_bundle
python3 "$assert_response" bind "$evidence_dir/12-enrollment-worker-binding.json" \
  "$expected_manifest" worker:fixture/enrollment

profile_request=$(python3 -c 'import json,sys; print(json.dumps({"profile_id":sys.argv[1]},separators=(",",":")))' \
  "$credential_profile")
run_json "$evidence_dir/13-credential-profile-create.json" --no-project execute \
  service:credential-profiles/create --no-stream --input "$profile_request"
python3 "$assert_response" profile-state \
  "$evidence_dir/13-credential-profile-create.json" "$credential_profile" unauthenticated

# Profile administration and session launch have distinct typed requests.
login_parameters=$(python3 -c 'import json,sys; print(json.dumps({"credential_profile_id":sys.argv[1]},separators=(",",":")))' \
  "$credential_profile")
login_json=$evidence_dir/14-enrollment-launch.json
run_json "$login_json" --no-project execute worker_execution:fixture/login \
  --async --no-stream --input "$login_parameters"
login_coordinate_text=$(python3 "$assert_response" launch "$login_json")
mapfile -t login_coordinate <<<"$login_coordinate_text"
login_thread=${login_coordinate[0]}
login_root=${login_coordinate[1]}

login_start_request=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1],"idempotency_key":"fixture-login-start-v1","route_id":"credential.login.start","payload":{}},separators=(",",":")))' \
  "$login_root")
run_json_retry "$evidence_dir/15-enrollment-start.json" --no-project execute \
  service:worker-executions/command --no-stream --input "$login_start_request"
login_start_coordinate_text=$(python3 "$assert_response" credential-command \
  "$evidence_dir/15-enrollment-start.json" start "$login_root" "$login_thread" 1)
mapfile -t login_start_coordinate <<<"$login_start_coordinate_text"
login_session_capsule=$(worker_session_capsule \
  "$evidence_dir/15-enrollment-authority" "$login_thread" "$login_root" \
  "${login_start_coordinate[1]}")
login_start_observation_request=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1],"placement_thread_id":sys.argv[2],"command_sequence":int(sys.argv[3])},separators=(",",":")))' \
  "${login_start_coordinate[0]}" "${login_start_coordinate[1]}" \
  "${login_start_coordinate[2]}")
# A successful command response is returned only after its observation batch,
# derived root facts, and settled command row are durable. Read that exact
# coordinate once; absence of the declared turn is a permanent contradiction.
run_json "$evidence_dir/15-enrollment-start-observation.json" --no-project execute \
  service:worker-executions/command-observation --no-stream \
  --input "$login_start_observation_request"
python3 "$assert_response" command-observation \
  "$evidence_dir/15-enrollment-start-observation.json" \
  "${login_start_coordinate[0]}" "${login_start_coordinate[1]}" \
  "${login_start_coordinate[2]}" fixture-login-start-v1 \
  "${login_start_coordinate[3]}" "${login_start_coordinate[4]}" \
  "$login_session_capsule" credential.login.start none

account_request=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1],"idempotency_key":"fixture-account-read-v1","route_id":"credential.account.read","payload":{}},separators=(",",":")))' \
  "$login_root")
run_json_retry "$evidence_dir/16-enrollment-account.json" --no-project execute \
  service:worker-executions/command --no-stream --input "$account_request"
account_coordinate_text=$(python3 "$assert_response" credential-command \
  "$evidence_dir/16-enrollment-account.json" account "$login_root" "$login_thread" 2)
mapfile -t account_coordinate <<<"$account_coordinate_text"
account_observation_request=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1],"placement_thread_id":sys.argv[2],"command_sequence":int(sys.argv[3])},separators=(",",":")))' \
  "${account_coordinate[0]}" "${account_coordinate[1]}" \
  "${account_coordinate[2]}")
run_json "$evidence_dir/16-enrollment-account-observation.json" --no-project execute \
  service:worker-executions/command-observation --no-stream \
  --input "$account_observation_request"
python3 "$assert_response" command-observation \
  "$evidence_dir/16-enrollment-account-observation.json" \
  "${account_coordinate[0]}" "${account_coordinate[1]}" \
  "${account_coordinate[2]}" fixture-account-read-v1 \
  "${account_coordinate[3]}" "${account_coordinate[4]}" \
  "$login_session_capsule" credential.account.read none
login_termination=$(python3 -c 'import json,sys; print(json.dumps({"chain_root_id":sys.argv[1],"reason":"cancelled"},separators=(",",":")))' \
  "$login_root")
run_json "$evidence_dir/17-enrollment-termination.json" --no-project execute \
  service:worker-executions/terminate --no-stream --input "$login_termination"
python3 "$assert_response" terminated "$evidence_dir/17-enrollment-termination.json" \
  "$login_root" cancelled

run_json "$evidence_dir/18-credential-profile-observed.json" --no-project execute \
  service:credential-profiles/get --no-stream --input "$profile_request"
observed_profile=$(python3 "$assert_response" profile-state \
  "$evidence_dir/18-credential-profile-observed.json" "$credential_profile" confirming)
mapfile -t observed_coordinate <<<"$observed_profile"
login_epoch=${observed_coordinate[0]}
account_digest=${observed_coordinate[1]}
confirm_request=$(python3 -c 'import json,sys; print(json.dumps({"profile_id":sys.argv[1],"login_epoch":int(sys.argv[2]),"expected_account_digest":sys.argv[3]},separators=(",",":")))' \
  "$credential_profile" "$login_epoch" "$account_digest")
run_json "$evidence_dir/19-credential-profile-confirm.json" --no-project execute \
  service:credential-profiles/confirm --no-stream --input "$confirm_request"
python3 "$assert_response" profile-state \
  "$evidence_dir/19-credential-profile-confirm.json" "$credential_profile" active

run_selected_worker_acceptance

printf '%s\n' \
  'qualified retained-product composition passed' \
  "wrapper_thread_id=$wrapper_thread" \
  "wrapper_chain_root_id=$wrapper_root" \
  "replay_wrapper_thread_id=$replay_thread" \
  "replay_wrapper_chain_root_id=$replay_root" \
  "distribution_witness_hash=$distribution_witness" \
  "runtime_witness_hash=$runtime_witness" \
  "verifier_thread_id=$verifier_thread" \
  "verifier_chain_root_id=$verifier_root" \
  "qualification_hash=$qualification_hash" \
  "qualification_coordinate_id=$qualification_coordinate_id" \
  "distribution_consumer_binding_hash=$distribution_consumer_binding" \
  "runtime_consumer_binding_hash=$runtime_consumer_binding" \
  "credential_profile_id=$credential_profile" \
  "enrollment_thread_id=$login_thread" \
  "enrollment_chain_root_id=$login_root" \
  "selected_worker_thread_id=$session_thread" \
  "selected_worker_chain_root_id=$session_root" \
  "evidence_directory=$evidence_dir" \
  'qualified selected-runtime worker execution passed with offline evidence'
