#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

project="$tmp/project"
mkdir "$project"
git -C "$project" init -q
git -C "$project" config user.email qualification@example.invalid
git -C "$project" config user.name Qualification
printf '%s\n' input > "$project/input.txt"
git -C "$project" add input.txt
git -C "$project" commit -qm fixture

source_app_root="$tmp/source-app-root"
mkdir -p "$source_app_root/.ai/config/keys/signing" "$source_app_root/.ai/node"
printf '%s\n' test-only-key > "$source_app_root/.ai/config/keys/signing/private_key.pem"
export RYEOS_APP_ROOT="$source_app_root"
export RYEOSD_URL="http://127.0.0.1:7400"

expected_hash="$(printf '%s\n' qualified | sha256sum | cut -d ' ' -f 1)"
fake_ryeos="$tmp/ryeos"
export FAKE_CALLS="$tmp/calls"
export FAKE_RESULT_MARKER="$tmp/executed"
cat > "$fake_ryeos" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

printf '%q ' "$@" >> "$FAKE_CALLS"
printf '\n' >> "$FAKE_CALLS"
args=" $* "
if [[ "$args" == *" node status "* ]]; then
    app_root=""
    while (( $# > 0 )); do
        if [[ "$1" == --app-root ]]; then app_root="$2"; break; fi
        shift
    done
    if [[ "${FAKE_STATUS_STALE:-0}" == 1 ]]; then
        printf '%s\n' '{"Stale":{"metadata":{},"diagnostics":{"message":"stale"}}}'
    else
        printf '{"Running":{"metadata":{"app_root":"%s","bind":"127.0.0.1:7400"}}}\n' "$app_root"
    fi
elif [[ "$args" == *" remote execute "* ]]; then
    project=""
    while (( $# > 0 )); do
        if [[ "$1" == --project ]]; then project="$2"; break; fi
        shift
    done
    : > "$FAKE_RESULT_MARKER"
    if [[ "${FAKE_EXECUTE_FAIL:-0}" == 1 ]]; then
        echo "simulated lost execute response" >&2
        exit 42
    fi
    if [[ "${FAKE_REPLACE_FSMONITOR:-0}" == 1 ]]; then
        printf '#!/usr/bin/env bash\ntouch %q\nexit 99\n' \
            "$FAKE_FSMONITOR_MARKER" > "$project/fsmonitor.sh"
        chmod +x "$project/fsmonitor.sh"
    fi
    if [[ "${FAKE_LEAVE_TRANSACTION_ARTIFACT:-0}" == 1 ]]; then
        : > "$project/.ryeos-pull.lock"
    fi
    printf '%s\n' qualified > "$project/result.txt"
    printf '%s\n' '{"job_id":"remote-execute:00000000-0000-4000-8000-000000000001","push":{"snapshot_hash":"sha256:source"},"remote":{"snapshot_hash":"sha256:result","result":{"status":"completed"}},"pull":{"snapshot_hash":"sha256:result","files_updated":1,"files_deleted":0}}'
elif [[ "$args" == *"service:sync/jobs/list"* ]]; then
    if [[ -e "$FAKE_RESULT_MARKER" ]]; then
        printf '%s\n' '{"jobs":[{"job_id":"remote-execute:00000000-0000-4000-8000-000000000001","operation_type":"remote_execute","state":"completed","uploaded_hashes":["sha256:source"],"fetched_hashes":["sha256:result"]}]}'
    else
        printf '%s\n' '{"jobs":[]}'
    fi
elif [[ "$args" == *"service:sync/jobs/inspect"* ]]; then
    printf '%s\n' '{"status":"found","job":{"job_id":"remote-execute:00000000-0000-4000-8000-000000000001","operation_type":"remote_execute","operation":{"item_ref":"tool:qualification/run","ref_bindings":{"model":"worker:models/qualified"},"target_site_id":"site:stronger","remote_project_path":"/srv/ryeos/projects/qualification","acting_principal":"operator:test"},"state":"completed","uploaded_hashes":["sha256:source"],"fetched_hashes":["sha256:result"]},"attempt_retention":{"mode":"complete","cumulative_count":1,"retained_count":1,"terminal_row_limit":null},"attempts":[{"attempt_id":"remote-execute-attempt:test","job_id":"remote-execute:00000000-0000-4000-8000-000000000001","attempt_number":1,"state":"completed"}]}'
elif [[ "$args" == *" remote doctor stronger "* ]]; then
    project=""
    while (( $# > 0 )); do
        if [[ "$1" == --project ]]; then project="$2"; break; fi
        shift
    done
    doctor_ok=true
    [[ "${FAKE_DOCTOR_FAIL:-0}" == 0 ]] || doctor_ok=false
    health_status=healthy
    [[ "${FAKE_DEGRADED:-0}" == 0 ]] || health_status=degraded
    identity_contract='"principal_id":"fp:stronger","configured_principal_id":"fp:stronger","configured_principal_matches":true,"site_id":"site:stronger","configured_site_id":"site:stronger","configured_site_matches":true,"vault_fingerprint":"vault:stronger","configured_vault_fingerprint":"vault:stronger","configured_vault_matches":true,"live_identity_binding_ok":true,"pinned_key_matches":true,"pinned_fingerprint_matches":true,"pinned_identity_matches":true'
    if [[ "${FAKE_IDENTITY_INCOMPLETE:-0}" == 1 ]]; then
        identity_contract='"site_id":"site:stronger","configured_site_id":"site:stronger","configured_site_matches":true'
    elif [[ "${FAKE_IDENTITY_MISMATCH:-0}" == 1 ]]; then
        identity_contract='"principal_id":"fp:stronger","configured_principal_id":"fp:stronger","configured_principal_matches":true,"site_id":"site:stronger","configured_site_id":"site:stronger","configured_site_matches":true,"vault_fingerprint":"vault:stronger","configured_vault_fingerprint":"vault:stale","configured_vault_matches":false,"live_identity_binding_ok":true,"pinned_key_matches":true,"pinned_fingerprint_matches":true,"pinned_identity_matches":false'
    fi
    printf '{"remote":{"health":{"status":"%s"}},"checks":[{"name":"remote_configured","ok":true},{"name":"remote_health","ok":%s},{"name":"remote_identity","ok":true,%s},{"name":"signed_authorization","ok":true},{"name":"project_binding","ok":true,"local_project_path":"%s","remote_project_path":"/srv/ryeos/projects/qualification","sync_scope":"full_project"}]}\n' "$health_status" "$doctor_ok" "$identity_contract" "$project"
elif [[ "$args" == *" remote list "* ]]; then
    printf '{"remotes":[{"name":"stronger","config_path":"%s/.ai/config/remotes/remotes.yaml"}],"invalid_remotes":[]}\n' "$RYEOS_APP_ROOT"
elif [[ "$args" == *" remote status stronger "* ]]; then
    printf '%s\n' '{"status":"ok"}'
else
    echo "unexpected fake ryeos invocation: $*" >&2
    exit 64
fi
EOF
chmod +x "$fake_ryeos"

bootstrap_project="$tmp/bootstrap-project"
mkdir -p "$bootstrap_project/bin"
bootstrap_bash_marker="$tmp/bootstrap-bash-executed"
bootstrap_cat_marker="$tmp/bootstrap-cat-executed"
bootstrap_env_marker="$tmp/bootstrap-bash-env-executed"
printf '#!/bin/sh\n: > %q\nexit 99\n' "$bootstrap_bash_marker" \
    > "$bootstrap_project/bin/bash"
printf '#!/bin/sh\n: > %q\nexit 99\n' "$bootstrap_cat_marker" \
    > "$bootstrap_project/bin/cat"
printf ': > %q\n' "$bootstrap_env_marker" > "$bootstrap_project/bash-env"
chmod +x "$bootstrap_project/bin/bash" "$bootstrap_project/bin/cat"
if PATH="$bootstrap_project/bin:$PATH" \
    BASH_ENV="$bootstrap_project/bash-env" \
    "$root/scripts/dev/qualify-configured-remote.sh" >/dev/null 2>&1; then
    echo "expected a parameterless bootstrap probe to print usage and fail" >&2
    exit 1
fi
test ! -e "$bootstrap_bash_marker"
test ! -e "$bootstrap_cat_marker"
test ! -e "$bootstrap_env_marker"
test ! -e "$FAKE_CALLS"

captured_root_project="$tmp/captured-root-project"
git clone -q "$project" "$captured_root_project"
printf '%s\n' client-root/ > "$captured_root_project/.gitignore"
git -C "$captured_root_project" add .gitignore
git -C "$captured_root_project" -c user.email=qualification@example.invalid \
    -c user.name=Qualification commit -qm ignore-client-root
captured_app_root="$captured_root_project/client-root"
mkdir -p "$captured_app_root/.ai/config/keys/signing" "$captured_app_root/.ai/node"
printf '%s\n' target-capturable-key \
    > "$captured_app_root/.ai/config/keys/signing/private_key.pem"
if RYEOS_APP_ROOT="$captured_app_root" RYEOSD_URL="http://127.0.0.1:7400" \
    "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$captured_root_project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/captured-root-rejected" \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected a client app root inside the synchronized project to be rejected" >&2
    exit 1
fi
test ! -e "$FAKE_CALLS"

project_evidence_parent="$project/uncreated-evidence-parent"
if RYEOS_APP_ROOT="$source_app_root" RYEOSD_URL="http://127.0.0.1:7400" \
    "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$project_evidence_parent/run" \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected a project-local evidence directory to be rejected" >&2
    exit 1
fi
test ! -e "$project_evidence_parent"
test ! -e "$FAKE_CALLS"

node_evidence_parent="$source_app_root/.ai/node/uncreated-evidence-parent"
if RYEOS_APP_ROOT="$source_app_root" RYEOSD_URL="http://127.0.0.1:7400" \
    "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$node_evidence_parent/run" \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected evidence inside the source app root to be rejected" >&2
    exit 1
fi
test ! -e "$node_evidence_parent"
test ! -e "$FAKE_CALLS"

if RYEOS_APP_ROOT="$source_app_root" RYEOSD_URL="http://192.0.2.10:7400" \
    "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/plaintext-remote-rejected" \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected non-loopback plaintext source transport to be rejected" >&2
    exit 1
fi
test ! -e "$FAKE_CALLS"

local_lifecycle_project="$tmp/local-lifecycle-project"
git clone -q "$project" "$local_lifecycle_project"
env -u RYEOSD_URL \
    RYEOS_APP_ROOT="$source_app_root" \
    FAKE_CALLS="$tmp/local-lifecycle-calls" \
    "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$local_lifecycle_project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/local-lifecycle-evidence" \
    --ryeos-bin "$fake_ryeos" \
    --ref-binding model=worker:models/qualified >/dev/null
grep -Fq "node status --app-root $source_app_root --json" \
    "$tmp/local-lifecycle-calls"
rm -f "$FAKE_RESULT_MARKER"

if env -u RYEOSD_URL \
    RYEOS_APP_ROOT="$source_app_root" \
    FAKE_CALLS="$tmp/stale-lifecycle-calls" \
    FAKE_STATUS_STALE=1 \
    "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/stale-lifecycle-rejected" \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected stale source lifecycle metadata to be rejected" >&2
    exit 1
fi
grep -Fq "node status --app-root $source_app_root --json" \
    "$tmp/stale-lifecycle-calls"
! grep -Fq " remote " "$tmp/stale-lifecycle-calls"

project_binary="$tmp/project-binary"
git clone -q "$project" "$project_binary"
cp "$fake_ryeos" "$project_binary/ryeos"
git -C "$project_binary" add ryeos
git -C "$project_binary" -c user.email=qualification@example.invalid \
    -c user.name=Qualification commit -qm project-binary
if "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project_binary" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/project-binary-rejected" \
    --ryeos-bin "$project_binary/ryeos" >/dev/null 2>&1; then
    echo "expected a RyeOS binary inside the synchronized project to be rejected" >&2
    exit 1
fi

ln -s "$fake_ryeos" "$tmp/ryeos-symlink"
if "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/symlink-binary-rejected" \
    --ryeos-bin "$tmp/ryeos-symlink" >/dev/null 2>&1; then
    echo "expected a symlinked RyeOS binary to be rejected" >&2
    exit 1
fi

ancestor_project="$tmp/project-realpath-ancestor"
git clone -q "$project" "$ancestor_project"
mkdir "$ancestor_project/bin"
printf '#!/usr/bin/env bash\ntouch %q\nexit 99\n' \
    "$tmp/ancestor-realpath-executed" > "$ancestor_project/bin/realpath"
chmod +x "$ancestor_project/bin/realpath"
git -C "$ancestor_project" add bin
git -C "$ancestor_project" -c user.email=qualification@example.invalid \
    -c user.name=Qualification commit -qm ancestor-realpath
ln -s "$ancestor_project/bin" "$tmp/outside-realpath-bin"
if PATH="$tmp/outside-realpath-bin:$PATH" \
    "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$ancestor_project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/ancestor-realpath-rejected" \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected a realpath reached through a project-bound ancestor symlink to be rejected" >&2
    exit 1
fi
test ! -e "$tmp/ancestor-realpath-executed"

tool_case=0
for shadowed_tool in python3 sha256sum; do
    tool_case=$((tool_case + 1))
    tool_project="$tmp/project-tool-$tool_case"
    git clone -q "$project" "$tool_project"
    mkdir "$tool_project/bin"
    printf '#!/usr/bin/env bash\ntouch %q\nexit 99\n' \
        "$tmp/shadowed-$shadowed_tool-executed" > "$tool_project/bin/$shadowed_tool"
    chmod +x "$tool_project/bin/$shadowed_tool"
    git -C "$tool_project" add bin
    git -C "$tool_project" -c user.email=qualification@example.invalid \
        -c user.name=Qualification commit -qm shadowed-tool
    if PATH="$tool_project/bin:$PATH" \
        "$root/scripts/dev/qualify-configured-remote.sh" \
        --remote stronger \
        --project "$tool_project" \
        --remote-project /srv/ryeos/projects/qualification \
        --item-ref tool:qualification/run \
        --evidence-dir "$tmp/project-tool-$tool_case-rejected" \
        --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
        echo "expected project-local $shadowed_tool to be rejected" >&2
        exit 1
    fi
    test ! -e "$tmp/shadowed-$shadowed_tool-executed"
done

export FAKE_DOCTOR_FAIL=1
if "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/doctor-failed" \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected a failed doctor check to reject the probe" >&2
    exit 1
fi
test "$(cat "$tmp/doctor-failed/status")" = failed
unset FAKE_DOCTOR_FAIL

export FAKE_DEGRADED=1
if "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/degraded" \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected a degraded target to reject the probe" >&2
    exit 1
fi
test "$(cat "$tmp/degraded/status")" = failed
unset FAKE_DEGRADED

transaction_artifact_project="$tmp/transaction-artifact-project"
git clone -q "$project" "$transaction_artifact_project"
export FAKE_LEAVE_TRANSACTION_ARTIFACT=1
if "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$transaction_artifact_project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/transaction-artifact-rejected" \
    --ryeos-bin "$fake_ryeos" \
    --ref-binding model=worker:models/qualified >/dev/null 2>&1; then
    echo "expected retained pull transaction residue to reject the probe" >&2
    exit 1
fi
unset FAKE_LEAVE_TRANSACTION_ARTIFACT
test "$(cat "$tmp/transaction-artifact-rejected/status")" = failed
test -s "$tmp/transaction-artifact-rejected/transaction-artifacts-after.nul"

identity_case=0
for identity_mode in FAKE_IDENTITY_INCOMPLETE FAKE_IDENTITY_MISMATCH; do
    identity_case=$((identity_case + 1))
    export "$identity_mode=1"
    if "$root/scripts/dev/qualify-configured-remote.sh" \
        --remote stronger \
        --project "$project" \
        --remote-project /srv/ryeos/projects/qualification \
        --item-ref tool:qualification/run \
        --evidence-dir "$tmp/identity-rejected-$identity_case" \
        --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
        echo "expected incomplete/mismatched doctor identity proof to be rejected" >&2
        exit 1
    fi
    unset "$identity_mode"
done

printf '%s\n' '{ "probe": true }' > "$tmp/input.json"
"$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/evidence" \
    --ryeos-bin "$fake_ryeos" \
    --input "$tmp/input.json" \
    --ref-binding model=worker:models/qualified \
    --expect-file "result.txt=$expected_hash"

test "$(cat "$tmp/evidence/status")" = passed
test ! -s "$tmp/evidence/transaction-artifacts-after.nul"
test "$(stat -c %a "$tmp/evidence")" = 700
grep -qx '{"probe":true}' "$tmp/evidence/input.json"
grep -qx remote-execute:00000000-0000-4000-8000-000000000001 "$tmp/evidence/remote-execute-job-id.txt"
grep -Fq '"ref_bindings":{"model":"worker:models/qualified"}' \
    "$tmp/evidence/probe-context.json"
grep -qx "{\"result.txt\":\"$expected_hash\"}" \
    "$tmp/evidence/expected-files.json"
grep -qx '{"result.txt":"missing"}' \
    "$tmp/evidence/expected-files-before.json"
source_app_root_digest="$(printf '%s' "$source_app_root" | sha256sum | cut -d ' ' -f 1)"
source_daemon_url_digest="$(printf '%s' "$RYEOSD_URL" | sha256sum | cut -d ' ' -f 1)"
grep -Fq '"app_root_digest":"'"$source_app_root_digest"'"' \
    "$tmp/evidence/source-client-authority.json"
grep -Fq '"daemon_url_digest":"'"$source_daemon_url_digest"'"' \
    "$tmp/evidence/source-client-authority.json"
! grep -Fq "$source_app_root" "$tmp/evidence/source-client-authority.json"
! grep -Fq "$RYEOSD_URL" "$tmp/evidence/source-client-authority.json"
! grep -Fq 'config_path' "$tmp/evidence/remote-list.json"
! grep -Fq "$source_app_root" "$tmp/evidence/remote-list.json"
(cd "$tmp/evidence" && sha256sum --check evidence.sha256)
grep -F -- "--project $project remote execute stronger tool:qualification/run" "$FAKE_CALLS"
grep -F -- '--ref-bindings \{\"model\":\"worker:models/qualified\"\}' "$FAKE_CALLS"
grep -F -- '--parameters \{\"probe\":true\}' "$FAKE_CALLS"
grep -F -- "--no-project execute service:sync/jobs/list" "$FAKE_CALLS"
! grep -Fq -- "remote execute stronger tool:qualification/run --no-stream" "$FAKE_CALLS"
! grep -Eq -- 'remote (list|status stronger|doctor stronger).*--no-stream' "$FAKE_CALLS"
! grep -Fq -- "remote bind-project" "$FAKE_CALLS"

fsmonitor_project="$tmp/fsmonitor-project"
git clone -q "$project" "$fsmonitor_project"
printf '#!/usr/bin/env bash\nexit 0\n' > "$fsmonitor_project/fsmonitor.sh"
chmod +x "$fsmonitor_project/fsmonitor.sh"
git -C "$fsmonitor_project" add fsmonitor.sh
git -C "$fsmonitor_project" -c user.email=qualification@example.invalid \
    -c user.name=Qualification commit -qm fsmonitor-fixture
git -C "$fsmonitor_project" config core.fsmonitor ./fsmonitor.sh
export FAKE_REPLACE_FSMONITOR=1
export FAKE_FSMONITOR_MARKER="$tmp/target-fsmonitor-executed"
"$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$fsmonitor_project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/fsmonitor-evidence" \
    --ryeos-bin "$fake_ryeos" \
    --ref-binding model=worker:models/qualified >/dev/null
unset FAKE_REPLACE_FSMONITOR
test ! -e "$FAKE_FSMONITOR_MARKER"

if "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$fsmonitor_project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/fsmonitor-second-run" \
    --ryeos-bin "$fake_ryeos" \
    --ref-binding model=worker:models/qualified >/dev/null 2>&1; then
    echo "expected the target-modified checkout to fail the clean-tree preflight" >&2
    exit 1
fi
test ! -e "$FAKE_FSMONITOR_MARKER"

for bad_binding in \
    'model=worker:models/one model=worker:models/two' \
    'BadName=worker:models/one'
do
    read -r -a bad_binding_args <<< "$bad_binding"
    binding_project="$tmp/binding-rejected-${bad_binding//[^a-zA-Z0-9]/_}"
    git clone -q "$project" "$binding_project"
    helper_args=()
    for binding in "${bad_binding_args[@]}"; do
        helper_args+=(--ref-binding "$binding")
    done
    if "$root/scripts/dev/qualify-configured-remote.sh" \
        --remote stronger \
        --project "$binding_project" \
        --remote-project /srv/ryeos/projects/qualification \
        --item-ref tool:qualification/run \
        --evidence-dir "$tmp/rejected-${bad_binding//[^a-zA-Z0-9]/_}" \
        --ryeos-bin "$fake_ryeos" \
        "${helper_args[@]}" >/dev/null 2>&1; then
        echo "expected invalid/duplicate binding to be rejected: $bad_binding" >&2
        exit 1
    fi
done

unsafe_expectation_case=0
for unsafe_expectation in \
    "result.txt=$expected_hash result.txt=$expected_hash" \
    $'control\tpath.txt='"$expected_hash"
do
    unsafe_expectation_case=$((unsafe_expectation_case + 1))
    read -r -a unsafe_expectation_args <<< "$unsafe_expectation"
    expectation_project="$tmp/expectation-rejected-$unsafe_expectation_case"
    git clone -q "$project" "$expectation_project"
    helper_args=()
    if [[ "$unsafe_expectation" == *$'\t'* ]]; then
        helper_args+=(--expect-file "$unsafe_expectation")
    else
        for expectation in "${unsafe_expectation_args[@]}"; do
            helper_args+=(--expect-file "$expectation")
        done
    fi
    if "$root/scripts/dev/qualify-configured-remote.sh" \
        --remote stronger \
        --project "$expectation_project" \
        --remote-project /srv/ryeos/projects/qualification \
        --item-ref tool:qualification/run \
        --evidence-dir "$tmp/unsafe-expectation-$unsafe_expectation_case" \
        --ryeos-bin "$fake_ryeos" \
        "${helper_args[@]}" >/dev/null 2>&1; then
        echo "expected ambiguous/control-character output evidence to be rejected" >&2
        exit 1
    fi
done

if "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$project" \
    --remote-project relative/path \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/rejected" \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected a relative target project path to be rejected" >&2
    exit 1
fi

for unsafe_path in '..' 'a/..' 'a/../../outside'; do
    if "$root/scripts/dev/qualify-configured-remote.sh" \
        --remote stronger \
        --project "$project" \
        --remote-project /srv/ryeos/projects/qualification \
        --item-ref tool:qualification/run \
        --evidence-dir "$tmp/unsafe-${unsafe_path//\//_}" \
        --ryeos-bin "$fake_ryeos" \
        --expect-file "$unsafe_path=$expected_hash" >/dev/null 2>&1; then
        echo "expected unsafe output path to be rejected: $unsafe_path" >&2
        exit 1
    fi
done

symlink_project="$tmp/symlink-project"
git clone -q "$project" "$symlink_project"
printf '%s\n' external > "$tmp/external.txt"
ln -s "$tmp/external.txt" "$symlink_project/leak.txt"
git -C "$symlink_project" add leak.txt
git -C "$symlink_project" -c user.email=qualification@example.invalid \
    -c user.name=Qualification commit -qm symlink-fixture
external_hash="$(sha256sum "$tmp/external.txt" | cut -d ' ' -f 1)"
if "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$symlink_project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/symlink-rejected" \
    --ryeos-bin "$fake_ryeos" \
    --expect-file "leak.txt=$external_hash" >/dev/null 2>&1; then
    echo "expected an output symlink to be rejected" >&2
    exit 1
fi
test "$(cat "$tmp/symlink-rejected/status")" = failed

failure_project="$tmp/failure-project"
git clone -q "$project" "$failure_project"
rm -f "$FAKE_RESULT_MARKER"
export FAKE_EXECUTE_FAIL=1
if "$root/scripts/dev/qualify-configured-remote.sh" \
    --remote stronger \
    --project "$failure_project" \
    --remote-project /srv/ryeos/projects/qualification \
    --item-ref tool:qualification/run \
    --evidence-dir "$tmp/execute-failed" \
    --ryeos-bin "$fake_ryeos" \
    --ref-binding model=worker:models/qualified >/dev/null 2>&1; then
    echo "expected the lost execute response to fail the probe" >&2
    exit 1
fi
unset FAKE_EXECUTE_FAIL
test "$(cat "$tmp/execute-failed/status")" = failed
grep -qx 42 "$tmp/execute-failed/remote-execute.exit"
grep -qx remote-execute:00000000-0000-4000-8000-000000000001 "$tmp/execute-failed/remote-execute-new-job-ids.txt"
test -s "$tmp/execute-failed/remote-execute-new-job-1-inspect.json"
(cd "$tmp/execute-failed" && sha256sum --check failure-evidence.sha256)

echo "configured remote round-trip probe contract ok"
