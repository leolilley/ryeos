#!/usr/bin/env bash
# Transfer the exact fixed product witnesses between two already-running,
# independently signed disposable nodes. This script never starts/stops a
# node, configures trust, signs source, discovers a product, or copies CAS.
set -euo pipefail

fixture_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
assert_response=$fixture_root/assert-live-response.py
expected_runtime_manifest=94c66e97825a25091abe924ae3de4715d26469332aa59f31cfb39854bdb59881
expected_distribution_manifest=1e3ec95d5f220c500598907f7647dd0a08dddf731fdf93da5be1bbc07b36f7b3
command_timeout=120
phase=fresh
origin_ryeos_bin=
origin_app_root=
origin_daemon_url=
origin_evidence_dir=
origin_snapshot=
origin_remote=
receiver_ryeos_bin=
receiver_app_root=
receiver_daemon_url=
receiver_snapshot=
evidence_dir=
resume_from=

usage() {
  printf '%s\n' \
    "usage (fresh): $0 --origin-ryeos-bin <absolute-path> --origin-app-root <absolute-path> \\" \
    '  --origin-daemon-url <url> --origin-evidence-dir <absolute-path> \' \
    '  --origin-producer-project-snapshot-hash <sha256> --origin-remote <configured-name> \' \
    '  --receiver-ryeos-bin <absolute-path> --receiver-app-root <absolute-path> \' \
    '  --receiver-daemon-url <url> --receiver-consumer-project-snapshot-hash <sha256> \' \
    '  --evidence-dir <new-directory> [--command-timeout <seconds>]' \
    "usage (after receiver restart, with origin unavailable): $0 --phase after-restart \\" \
    '  --receiver-ryeos-bin <absolute-path> --receiver-app-root <absolute-path> \' \
    '  --receiver-daemon-url <url> --receiver-consumer-project-snapshot-hash <sha256> \' \
    '  --resume-from <absolute-prior-evidence-directory> --evidence-dir <new-directory>'
}

while (($#)); do
  case $1 in
    --phase) phase=${2-}; shift 2 ;;
    --origin-ryeos-bin) origin_ryeos_bin=${2-}; shift 2 ;;
    --origin-app-root) origin_app_root=${2-}; shift 2 ;;
    --origin-daemon-url) origin_daemon_url=${2-}; shift 2 ;;
    --origin-evidence-dir) origin_evidence_dir=${2-}; shift 2 ;;
    --origin-producer-project-snapshot-hash) origin_snapshot=${2-}; shift 2 ;;
    --origin-remote) origin_remote=${2-}; shift 2 ;;
    --receiver-ryeos-bin) receiver_ryeos_bin=${2-}; shift 2 ;;
    --receiver-app-root) receiver_app_root=${2-}; shift 2 ;;
    --receiver-daemon-url) receiver_daemon_url=${2-}; shift 2 ;;
    --receiver-consumer-project-snapshot-hash) receiver_snapshot=${2-}; shift 2 ;;
    --evidence-dir) evidence_dir=${2-}; shift 2 ;;
    --resume-from) resume_from=${2-}; shift 2 ;;
    --command-timeout) command_timeout=${2-}; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 64 ;;
  esac
done

[[ $phase == fresh || $phase == after-restart ]] || {
  printf 'unsupported portability phase: %s\n' "$phase" >&2
  exit 64
}
for value in "$receiver_ryeos_bin" "$receiver_app_root" "$evidence_dir"; do
  [[ $value == /* ]] || { printf 'receiver coordinates must be absolute: %s\n' "$value" >&2; exit 64; }
done
[[ -x $receiver_ryeos_bin && -d $receiver_app_root ]] || {
  printf '%s\n' 'receiver binary or app root is unavailable' >&2
  exit 66
}
[[ $receiver_daemon_url =~ ^https?://[^[:space:]]+$ ]] || {
  printf '%s\n' 'receiver daemon URL must be explicit HTTP(S)' >&2
  exit 64
}
[[ $receiver_snapshot =~ ^[0-9a-f]{64}$ ]] || {
  printf '%s\n' 'receiver snapshot must be a lowercase SHA-256 digest' >&2
  exit 64
}
[[ $command_timeout =~ ^[1-9][0-9]*$ ]] || {
  printf '%s\n' 'command timeout must be positive seconds' >&2
  exit 64
}
[[ ! -e $evidence_dir ]] || { printf 'evidence directory exists: %s\n' "$evidence_dir" >&2; exit 73; }
/usr/bin/mkdir -p -- "$evidence_dir"

run_at() {
  local binary=$1 app_root=$2 daemon_url=$3 destination=$4
  shift 4
  if RYEOS_APP_ROOT=$app_root RYEOSD_URL=$daemon_url \
    timeout "$command_timeout" "$binary" "$@" >"$destination"
  then
    python3 -c 'import json,sys; json.load(open(sys.argv[1],encoding="utf-8"))' "$destination"
  else
    local status=$?
    printf 'command failed (%s), response retained at %s\n' "$status" "$destination" >&2
    return "$status"
  fi
}

run_receiver() {
  local destination=$1
  shift
  run_at "$receiver_ryeos_bin" "$receiver_app_root" "$receiver_daemon_url" \
    "$destination" "$@"
}

run_execution() {
  local destination=$1
  shift
  local launch=$destination.launch.json
  run_receiver "$launch" "$@" --async --no-stream --input '{}'
  mapfile -t coordinate < <(python3 "$assert_response" launch "$launch")
  local thread=${coordinate[0]} root=${coordinate[1]}
  local deadline=$((SECONDS + command_timeout)) attempt=0
  while true; do
    attempt=$((attempt + 1))
    local observed=$destination.observation-$(printf '%04d' "$attempt").json
    run_receiver "$observed" thread get --thread-id "$thread"
    local disposition
    disposition=$(python3 "$assert_response" terminal-disposition "$observed" "$thread" "$root")
    if [[ $disposition == completed ]]; then
      /usr/bin/cp -- "$observed" "$destination"
      execution_thread=$thread
      execution_root=$root
      return 0
    fi
    ((SECONDS < deadline)) || {
      printf 'accepted verifier remains pending: %s\n' "$launch" >&2
      return 1
    }
    sleep 1
  done
}

run_receiver "$evidence_dir/00-receiver-identity.json" --no-project execute \
  service:identity/public_key --no-stream --input '{}'
receiver_identity=$(python3 "$assert_response" node-identity "$evidence_dir/00-receiver-identity.json")

if [[ $phase == fresh ]]; then
  for value in "$origin_ryeos_bin" "$origin_app_root" "$origin_evidence_dir"; do
    [[ $value == /* ]] || { printf 'origin coordinates must be absolute: %s\n' "$value" >&2; exit 64; }
  done
  [[ -x $origin_ryeos_bin && -d $origin_app_root && -d $origin_evidence_dir ]] || {
    printf '%s\n' 'origin binary, app root, or exact evidence is unavailable' >&2
    exit 66
  }
  [[ $origin_daemon_url =~ ^https?://[^[:space:]]+$ && $origin_snapshot =~ ^[0-9a-f]{64}$ ]] || {
    printf '%s\n' 'origin URL and producer snapshot must be exact' >&2
    exit 64
  }
  [[ $origin_remote =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$ ]] || {
    printf '%s\n' 'configured origin remote name is invalid' >&2
    exit 64
  }
  accepted=$origin_evidence_dir/01-recorded-wrapper.json
  [[ -f $accepted ]] || { printf 'origin accepted-result evidence is absent: %s\n' "$accepted" >&2; exit 66; }
  mapfile -t witnesses < <(python3 "$assert_response" accepted "$accepted" "$origin_snapshot")
  distribution_witness=${witnesses[0]}
  runtime_witness=${witnesses[1]}

  run_at "$origin_ryeos_bin" "$origin_app_root" "$origin_daemon_url" \
    "$evidence_dir/01-origin-identity.json" --no-project execute \
    service:identity/public_key --no-stream --input '{}'
  origin_identity=$(python3 "$assert_response" node-identity "$evidence_dir/01-origin-identity.json")
  [[ $origin_identity != "$receiver_identity" ]] || {
    printf '%s\n' 'portability requires distinct origin and receiver node identities' >&2
    exit 65
  }

  for product in distribution runtime; do
    if [[ $product == distribution ]]; then witness=$distribution_witness; else witness=$runtime_witness; fi
    request=$(python3 -c 'import json,sys; print(json.dumps({"subject_hash":sys.argv[1],"policy":"local-node-v2","claim":"accepted"},separators=(",",":")))' "$witness")
    run_at "$origin_ryeos_bin" "$origin_app_root" "$origin_daemon_url" \
      "$evidence_dir/02-origin-$product-admission.json" --no-project execute \
      service:admission/submit --no-stream --input "$request"
  done
  distribution_admission=$(python3 "$assert_response" origin-admission \
    "$evidence_dir/02-origin-distribution-admission.json" "$distribution_witness")
  runtime_admission=$(python3 "$assert_response" origin-admission \
    "$evidence_dir/02-origin-runtime-admission.json" "$runtime_witness")

  run_receiver "$evidence_dir/03-receive-distribution.json" external-content \
    receive-product "$origin_remote" "$distribution_witness" "$distribution_admission" 32768
  distribution_acceptance=$(python3 "$assert_response" received-product \
    "$evidence_dir/03-receive-distribution.json" "$distribution_witness" \
    "$distribution_admission" "$expected_distribution_manifest")
  run_receiver "$evidence_dir/04-receive-runtime.json" external-content \
    receive-product "$origin_remote" "$runtime_witness" "$runtime_admission" 16384
  runtime_acceptance=$(python3 "$assert_response" received-product \
    "$evidence_dir/04-receive-runtime.json" "$runtime_witness" \
    "$runtime_admission" "$expected_runtime_manifest")
else
  [[ $resume_from == /* && -d $resume_from ]] || {
    printf '%s\n' 'after-restart requires an absolute prior evidence directory' >&2
    exit 64
  }
  mapfile -t retained < <(python3 - "$resume_from/summary.json" "$receiver_snapshot" "$receiver_identity" <<'PY'
import json, re, sys
value=json.load(open(sys.argv[1],encoding="utf-8"))
if value.get("schema") != "ryeos.fixture.product_portability.v1": raise SystemExit("wrong portability summary")
if value.get("receiver_snapshot") != sys.argv[2]: raise SystemExit("receiver snapshot changed across restart")
if value.get("receiver_identity") != sys.argv[3]: raise SystemExit("receiver node identity changed across restart")
fields=("distribution_witness","runtime_witness","distribution_admission","runtime_admission",
        "distribution_acceptance","runtime_acceptance","qualification_hash",
        "qualification_coordinate","verifier_thread","verifier_root",
        "distribution_binding","runtime_binding",
        "pre_selection_digest","selected_digest")
for field in fields:
    item=value.get(field)
    if not isinstance(item,str): raise SystemExit(f"missing {field}")
    if field in {"verifier_thread","verifier_root"} and not item.startswith("T-"):
        raise SystemExit(f"invalid {field}")
    if field not in {"verifier_thread","verifier_root"} and not re.fullmatch(r"[0-9a-f]{64}",item):
        raise SystemExit(f"invalid {field}")
    print(item)
PY
  )
  distribution_witness=${retained[0]}; runtime_witness=${retained[1]}
  distribution_admission=${retained[2]}; runtime_admission=${retained[3]}
  distribution_acceptance=${retained[4]}; runtime_acceptance=${retained[5]}
  prior_qualification=${retained[6]}; prior_qualification_coordinate=${retained[7]}
  prior_verifier_thread=${retained[8]}; prior_verifier_root=${retained[9]}
  prior_distribution_binding=${retained[10]}; prior_runtime_binding=${retained[11]}
  prior_pre_selection=${retained[12]}; prior_selected=${retained[13]}
fi

distribution_source=$(python3 -c 'import json,sys; print(json.dumps({"kind":"received","acceptance_hash":sys.argv[1]},separators=(",",":")))' "$distribution_acceptance")
runtime_source=$(python3 -c 'import json,sys; print(json.dumps({"kind":"received","acceptance_hash":sys.argv[1]},separators=(",",":")))' "$runtime_acceptance")

# Fresh receiver eligibility is required even after restart. This checks the
# exact current receiver acceptance before the existing import/bind owner.
run_receiver "$evidence_dir/05-runtime-import.json" external-content \
  import-product "$runtime_witness" "$runtime_source" 16384
mapfile -t imported < <(python3 "$assert_response" import \
  "$evidence_dir/05-runtime-import.json" "$expected_runtime_manifest")
run_receiver "$evidence_dir/06-verifier-binding.json" --no-project external-content bind \
  "${imported[0]}" "${imported[1]}" "$expected_runtime_manifest" \
  tool:test/verify-runtime installed_bundle
python3 "$assert_response" bind "$evidence_dir/06-verifier-binding.json" "$expected_runtime_manifest"

if [[ $phase == fresh ]]; then
  run_execution "$evidence_dir/07-verifier-thread.json" \
    --no-project execute tool:test/verify-runtime
  verifier_thread=$execution_thread; verifier_root=$execution_root
  python3 "$assert_response" verifier-result "$evidence_dir/07-verifier-thread.json" \
    "$expected_runtime_manifest"
else
  verifier_thread=$prior_verifier_thread; verifier_root=$prior_verifier_root
  run_receiver "$evidence_dir/07-verifier-thread.json" thread get --thread-id "$verifier_thread"
  python3 "$assert_response" thread "$evidence_dir/07-verifier-thread.json" \
    "$verifier_thread" "$verifier_root"
  python3 "$assert_response" verifier-result "$evidence_dir/07-verifier-thread.json" \
    "$expected_runtime_manifest"
fi

run_receiver "$evidence_dir/08-qualification.json" external-content qualify-product \
  "$runtime_witness" "$runtime_source" runtime_to_consumer "$verifier_root" "$verifier_thread"
if [[ $phase == fresh ]]; then
  mapfile -t qualified < <(python3 "$assert_response" qualification \
    "$evidence_dir/08-qualification.json")
else
  mapfile -t qualified < <(python3 "$assert_response" qualification-idempotent \
    "$evidence_dir/08-qualification.json" "$prior_qualification" \
    "$prior_qualification_coordinate")
fi
qualification_hash=${qualified[0]}
qualification_coordinate=${qualified[1]}
if [[ $phase == after-restart \
  && ( $qualification_hash != "$prior_qualification" \
    || $qualification_coordinate != "$prior_qualification_coordinate" ) ]]
then
  printf '%s\n' 'restart changed the exact qualification identity' >&2
  exit 65
fi

compose_request=$(python3 -c 'import json,sys; print(json.dumps({"consumer_ref":"config:test/runtime-consumer","project_context":{"snapshot_hash":sys.argv[1]},"selections":[{"declaration_id":"distribution","witness_hash":sys.argv[2],"witness_source":{"kind":"received","acceptance_hash":sys.argv[3]},"qualification_hash":None},{"declaration_id":"runtime","witness_hash":sys.argv[4],"witness_source":{"kind":"received","acceptance_hash":sys.argv[5]},"qualification_hash":sys.argv[6]}],"maximum_bytes":32768},separators=(",",":")))' "$receiver_snapshot" "$distribution_witness" "$distribution_acceptance" "$runtime_witness" "$runtime_acceptance" "$qualification_hash")
run_receiver "$evidence_dir/09-composition.json" external-content \
  compose-product "$compose_request"
mapfile -t composition < <(python3 "$assert_response" received-composition \
  "$evidence_dir/09-composition.json" \
  "$receiver_snapshot" "$distribution_witness" "$runtime_witness" "$qualification_hash" \
  "$expected_distribution_manifest" "$expected_runtime_manifest" \
  "$distribution_acceptance" "$runtime_acceptance")
distribution_binding=${composition[0]}; runtime_binding=${composition[1]}
pre_selection_digest=${composition[2]}; selected_digest=${composition[3]}
if [[ $phase == after-restart ]] \
  && [[ $distribution_binding != "$prior_distribution_binding" \
    || $runtime_binding != "$prior_runtime_binding" \
    || $pre_selection_digest != "$prior_pre_selection" \
    || $selected_digest != "$prior_selected" ]]
then
  printf '%s\n' 'restart changed exact received composition authority' >&2
  exit 65
fi

python3 - "$evidence_dir/summary.json" <<PY
import json,sys
value={
 "schema":"ryeos.fixture.product_portability.v1",
 "receiver_identity":"$receiver_identity", "receiver_snapshot":"$receiver_snapshot",
 "distribution_witness":"$distribution_witness", "runtime_witness":"$runtime_witness",
 "distribution_admission":"$distribution_admission", "runtime_admission":"$runtime_admission",
 "distribution_acceptance":"$distribution_acceptance", "runtime_acceptance":"$runtime_acceptance",
 "qualification_hash":"$qualification_hash", "qualification_coordinate":"$qualification_coordinate",
 "verifier_thread":"$verifier_thread",
 "verifier_root":"$verifier_root", "distribution_binding":"$distribution_binding",
 "runtime_binding":"$runtime_binding", "pre_selection_digest":"$pre_selection_digest",
 "selected_digest":"$selected_digest"
}
with open(sys.argv[1],"x",encoding="utf-8") as out:
    json.dump(value,out,sort_keys=True,separators=(",",":")); out.write("\n")
PY

printf 'product portability %s phase passed; exact evidence: %s\n' "$phase" "$evidence_dir"
