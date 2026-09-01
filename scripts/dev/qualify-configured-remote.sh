#!/bin/bash -p
# Exercise one already-configured RyeOS remote through the generic full-project
# push -> execute -> pull path and retain a compact operational evidence set.

set -euo pipefail
umask 077
unset BASH_ENV ENV CDPATH
unset PYTHONHOME PYTHONPATH PYTHONSTARTUP
unset LD_AUDIT LD_DEBUG LD_DEBUG_OUTPUT LD_LIBRARY_PATH LD_PRELOAD LD_PROFILE
unset GCONV_PATH LOCPATH
unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_CEILING_DIRECTORIES GIT_DIR
unset GIT_EXEC_PATH GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_TEMPLATE_DIR
unset GIT_WORK_TREE GIT_CONFIG GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM
GIT_CONFIG_COUNT=0
GIT_CONFIG_GLOBAL=/dev/null
GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_COUNT GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM

usage() {
    printf '%s\n' \
        'usage: qualify-configured-remote.sh \' \
        '  --remote NAME \' \
        '  --project DIR \' \
        '  --remote-project ABSOLUTE_PATH \' \
        '  --item-ref REF \' \
        '  --evidence-dir DIR \' \
        '  [--ryeos-bin PATH] \' \
        '  [--input FILE] \' \
        '  [--ref-binding NAME=REF]... \' \
        '  [--expect-file RELATIVE_PATH=SHA256]...' \
        '' \
        'The source and target nodes must already be running, mutually authorized, and' \
        'the named remote and exact full-project binding must already be configured.' \
        'RYEOS_APP_ROOT must name the exact initialized source node used by the RyeOS' \
        'client. The helper canonicalizes and freezes that root and its daemon endpoint' \
        'before contacting RyeOS.' \
        "The script never provisions, installs, starts, stops, or changes either node's" \
        'identity, authorization, or project bindings.' >&2
    exit 2
}

remote=""
project=""
remote_project=""
item_ref=""
evidence_dir=""
ryeos_bin="${RYEOS_BIN:-ryeos}"
input_file=""
ref_bindings=()
expected_files=()

while (( $# > 0 )); do
    case "$1" in
        --remote) remote="${2:-}"; shift 2 ;;
        --project) project="${2:-}"; shift 2 ;;
        --remote-project) remote_project="${2:-}"; shift 2 ;;
        --item-ref) item_ref="${2:-}"; shift 2 ;;
        --evidence-dir) evidence_dir="${2:-}"; shift 2 ;;
        --ryeos-bin) ryeos_bin="${2:-}"; shift 2 ;;
        --input) input_file="${2:-}"; shift 2 ;;
        --ref-binding) ref_bindings+=("${2:-}"); shift 2 ;;
        --expect-file) expected_files+=("${2:-}"); shift 2 ;;
        *) usage ;;
    esac
done

[[ -n "$remote" && -n "$project" && -n "$remote_project" ]] || usage
[[ -n "$item_ref" && -n "$evidence_dir" ]] || usage
[[ -n "${RYEOS_APP_ROOT:-}" ]] || {
    echo "RYEOS_APP_ROOT must explicitly name the source RyeOS node" >&2
    exit 2
}
[[ "$RYEOS_APP_ROOT" == /* ]] || {
    echo "RYEOS_APP_ROOT must be an absolute path" >&2
    exit 2
}
[[ "$remote" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || {
    echo "invalid configured remote name: $remote" >&2
    exit 2
}
[[ "$item_ref" =~ ^[a-z][a-z0-9_-]*:[^[:space:]]+$ ]] || {
    echo "invalid canonical item ref: $item_ref" >&2
    exit 2
}
[[ "$remote_project" == /* ]] || {
    echo "--remote-project must be an absolute target-local path" >&2
    exit 2
}
[[ -d "$project" ]] || {
    echo "project does not exist: $project" >&2
    exit 2
}

project="$(cd -- "$project" && pwd -P)"

realpath_candidate="$(command -v -- realpath)" || {
    echo "realpath is required" >&2
    exit 2
}
[[ "$realpath_candidate" == /* && -f "$realpath_candidate" && -x "$realpath_candidate" && ! -L "$realpath_candidate" ]] || {
    echo "realpath must resolve to an absolute regular executable, not a symlink" >&2
    exit 2
}
realpath_directory="${realpath_candidate%/*}"
realpath_name="${realpath_candidate##*/}"
physical_realpath_directory="$(
    unset CDPATH
    cd -P -- "$realpath_directory"
    pwd -P
)" || {
    echo "realpath executable directory cannot be physically resolved" >&2
    exit 2
}
realpath_bin="$physical_realpath_directory/$realpath_name"
[[ -f "$realpath_bin" && -x "$realpath_bin" && ! -L "$realpath_bin" ]] || {
    echo "realpath physical target must be a regular executable, not a symlink" >&2
    exit 2
}
case "$realpath_bin" in
    "$project"|"$project/"*)
        echo "realpath must be outside the synchronized project" >&2
        exit 2
        ;;
esac

pin_external_command() {
    local command_name="$1"
    local output_name="$2"
    local reject_symlink="${3:-false}"
    local candidate resolved
    candidate="$(command -v -- "$command_name")" || {
        echo "$command_name is required" >&2
        exit 2
    }
    [[ -f "$candidate" && -x "$candidate" ]] || {
        echo "$command_name is not a regular executable: $candidate" >&2
        exit 2
    }
    if [[ "$reject_symlink" == true && -L "$candidate" ]]; then
        echo "$command_name must not be selected through a symlink: $candidate" >&2
        exit 2
    fi
    resolved="$("$realpath_bin" -e -- "$candidate")"
    [[ "$resolved" == /* && -f "$resolved" && -x "$resolved" ]] || {
        echo "$command_name did not resolve to a regular executable" >&2
        exit 2
    }
    case "$resolved" in
        "$project"|"$project/"*)
            echo "$command_name must be outside the synchronized project: $resolved" >&2
            exit 2
            ;;
    esac
    printf -v "$output_name" '%s' "$resolved"
}

bash_bin=""
git_bin=""
python3_bin=""
sha256sum_bin=""
cut_bin=""
wc_bin=""
find_bin=""
sort_bin=""
xargs_bin=""
dirname_bin=""
basename_bin=""
pin_external_command bash bash_bin
pin_external_command git git_bin
pin_external_command python3 python3_bin
pin_external_command sha256sum sha256sum_bin
pin_external_command cut cut_bin
pin_external_command wc wc_bin
pin_external_command find find_bin
pin_external_command sort sort_bin
pin_external_command xargs xargs_bin
pin_external_command dirname dirname_bin
pin_external_command basename basename_bin
pin_external_command "$ryeos_bin" ryeos_bin true

trusted_path=""
for trusted_binary in \
    "$bash_bin" "$git_bin" "$python3_bin" "$realpath_bin" "$sha256sum_bin" \
    "$cut_bin" "$wc_bin" "$find_bin" "$sort_bin" "$xargs_bin" \
    "$dirname_bin" "$basename_bin" "$ryeos_bin"
do
    trusted_directory="${trusted_binary%/*}"
    case ":$trusted_path:" in
        *":$trusted_directory:"*) ;;
        *) trusted_path="${trusted_path:+$trusted_path:}$trusted_directory" ;;
    esac
done
PATH="$trusted_path"
export PATH

[[ -d "$RYEOS_APP_ROOT" ]] || {
    echo "source RyeOS app root does not exist: $RYEOS_APP_ROOT" >&2
    exit 2
}
source_app_root="$("$realpath_bin" -e -- "$RYEOS_APP_ROOT")"
case "$source_app_root" in
    "$project"|"$project/"*)
        echo "source RyeOS app root must be outside the synchronized project" >&2
        exit 2
        ;;
esac

require_source_authority_directory() {
    local label="$1"
    local candidate="$2"
    local resolved
    [[ -d "$candidate" ]] || {
        echo "$label does not exist as a directory: $candidate" >&2
        exit 2
    }
    resolved="$("$realpath_bin" -e -- "$candidate")"
    case "$resolved" in
        "$project"|"$project/"*)
            echo "$label must be outside the synchronized project" >&2
            exit 2
            ;;
    esac
}

source_ai_root="$source_app_root/.ai"
source_config_root="$source_ai_root/config"
source_node_config_root="$source_ai_root/node"
source_signing_key="$source_config_root/keys/signing/private_key.pem"
require_source_authority_directory "source RyeOS authority root" "$source_ai_root"
require_source_authority_directory "source RyeOS operator configuration" "$source_config_root"
require_source_authority_directory "source RyeOS node configuration" "$source_node_config_root"
[[ -f "$source_signing_key" && ! -L "$source_signing_key" ]] || {
    echo "source RyeOS operator signing key must be a regular non-symlink file" >&2
    exit 2
}
resolved_source_signing_key="$("$realpath_bin" -e -- "$source_signing_key")"
case "$resolved_source_signing_key" in
    "$project"|"$project/"*)
        echo "source RyeOS operator signing key must be outside the synchronized project" >&2
        exit 2
        ;;
esac

RYEOS_APP_ROOT="$source_app_root"
export RYEOS_APP_ROOT

normalize_source_daemon_url() {
    "$python3_bin" -I -S - "$1" <<'PY'
import ipaddress
import sys
import urllib.parse

raw = sys.argv[1]
if not raw or any(ord(character) < 32 or ord(character) == 127 for character in raw):
    raise SystemExit("source RyeOS daemon URL is empty or contains a control character")
normalized = raw.rstrip("/")
parsed = urllib.parse.urlsplit(normalized)
if parsed.scheme not in ("http", "https") or not parsed.netloc:
    raise SystemExit("source RyeOS daemon URL must be an absolute HTTP(S) URL")
if parsed.username is not None or parsed.password is not None:
    raise SystemExit("source RyeOS daemon URL must not contain credentials")
if parsed.query or parsed.fragment:
    raise SystemExit("source RyeOS daemon base URL must not contain a query or fragment")
host = parsed.hostname
if host is None:
    raise SystemExit("source RyeOS daemon URL has no host")
try:
    loopback = ipaddress.ip_address(host).is_loopback
except ValueError:
    loopback = host.lower() == "localhost"
if parsed.scheme != "https" and not loopback:
    raise SystemExit("source RyeOS daemon URL must use HTTPS except HTTP loopback")
print(normalized)
PY
}

live_source_daemon_url() {
    local lifecycle_json
    lifecycle_json="$(
        unset RYEOSD_URL
        "$ryeos_bin" node status --app-root "$source_app_root" --json
    )"
    "$python3_bin" -I -S - "$lifecycle_json" "$source_app_root" <<'PY'
import json
import sys

status = json.loads(sys.argv[1])
running = status.get("Running")
if not isinstance(running, dict):
    raise SystemExit("source RyeOS node is not in the running lifecycle state")
metadata = running.get("metadata")
if not isinstance(metadata, dict) or metadata.get("app_root") != sys.argv[2]:
    raise SystemExit("source RyeOS lifecycle identity does not match RYEOS_APP_ROOT")
bind = metadata.get("bind")
if not isinstance(bind, str) or not bind:
    raise SystemExit("running source RyeOS lifecycle has no HTTP bind address")
print("http://" + bind)
PY
}

if [[ -n "${RYEOSD_URL+x}" ]]; then
    source_daemon_mode="explicit"
    source_daemon_url="$(normalize_source_daemon_url "$RYEOSD_URL")"
else
    source_daemon_mode="local_lifecycle"
    source_daemon_url="$(normalize_source_daemon_url "$(live_source_daemon_url)")"
fi

# Freeze both authorities for every pre- and post-pull invocation. The target
# can replace the project, but it cannot redirect the client to a captured key,
# changed node configuration, or a different source daemon.
RYEOSD_URL="$source_daemon_url"
export RYEOSD_URL

run_ryeos() {
    if [[ "$source_daemon_mode" == local_lifecycle ]]; then
        local live_url
        live_url="$(normalize_source_daemon_url "$(live_source_daemon_url)")"
        [[ "$live_url" == "$source_daemon_url" ]] || {
            echo "source RyeOS lifecycle bind changed during qualification" >&2
            return 1
        }
    fi
    "$ryeos_bin" "$@"
}

evidence_parent="$("$dirname_bin" "$evidence_dir")"
[[ -d "$evidence_parent" ]] || {
    echo "evidence parent must already exist: $evidence_parent" >&2
    exit 2
}
evidence_parent="$("$realpath_bin" -e -- "$evidence_parent")"
evidence_dir="$evidence_parent/$("$basename_bin" "$evidence_dir")"
case "$evidence_dir" in
    "$project"|"$project/"*)
        echo "evidence directory must be outside the synchronized project" >&2
        exit 2
        ;;
    "$source_app_root"|"$source_app_root/"*)
        echo "evidence directory must be outside the source RyeOS app root" >&2
        exit 2
        ;;
esac
if [[ -e "$evidence_dir" || -L "$evidence_dir" ]]; then
    echo "evidence directory already exists: $evidence_dir" >&2
    exit 2
fi
mkdir "$evidence_dir"
status_file="$evidence_dir/status"
printf '%s\n' running > "$status_file"
finish() {
    result=$?
    if (( result == 0 )); then
        printf '%s\n' passed > "$status_file"
    else
        printf '%s\n' failed > "$status_file"
        echo "remote round-trip probe failed; evidence retained at $evidence_dir" >&2
    fi
    exit "$result"
}
trap finish EXIT

if [[ -n "$input_file" ]]; then
    [[ -f "$input_file" ]] || {
        echo "input file does not exist: $input_file" >&2
        exit 2
    }
    input_source="$("$realpath_bin" "$input_file")"
else
    input_source=""
fi
if [[ -n "$input_source" ]]; then
    input_json="$("$python3_bin" -I -S - "$input_source" <<'PY'
import json
import os
import sys

path = sys.argv[1]
if os.path.getsize(path) > 1024 * 1024:
    raise SystemExit("qualification input exceeds the 1 MiB command bound")
with open(path, encoding="utf-8") as stream:
    value = json.load(stream)
if not isinstance(value, dict):
    raise SystemExit("qualification input must be one JSON object")
print(json.dumps(value, sort_keys=True, separators=(",", ":")))
PY
)"
else
    input_json='{}'
fi
input_file="$evidence_dir/input.json"
printf '%s\n' "$input_json" > "$input_file"

for binding in "${ref_bindings[@]}"; do
    [[ "$binding" == *=* && "$binding" != =* && "$binding" != *= ]] || {
        echo "invalid --ref-binding (expected NAME=REF): $binding" >&2
        exit 2
    }
done
ref_bindings_json="$("$python3_bin" -I -S - "${ref_bindings[@]}" <<'PY'
import json
import re
import sys

name_pattern = re.compile(r"[a-z][a-z0-9]*(?:_[a-z0-9]+)*")
ref_pattern = re.compile(r"[a-z][a-z0-9_-]*:\S+")
bindings = {}
for raw in sys.argv[1:]:
    name, item_ref = raw.split("=", 1)
    if not name_pattern.fullmatch(name) or len(name) > 64:
        raise SystemExit(f"invalid ref binding name: {name}")
    if not ref_pattern.fullmatch(item_ref) or len(item_ref) > 2048:
        raise SystemExit(f"invalid canonical ref binding for {name}")
    if name in bindings:
        raise SystemExit(f"duplicate ref binding name: {name}")
    bindings[name] = item_ref
print(json.dumps(bindings, sort_keys=True, separators=(",", ":")))
PY
)"
"$python3_bin" -I -S - "$evidence_dir/expected-files.json" "${expected_files[@]}" <<'PY'
import json
import re
import sys

output_path = sys.argv[1]
expected = {}
for raw in sys.argv[2:]:
    if "=" not in raw:
        raise SystemExit(f"invalid --expect-file (expected RELATIVE_PATH=SHA256): {raw}")
    path, digest = raw.rsplit("=", 1)
    if not path or not digest:
        raise SystemExit(f"invalid --expect-file (expected RELATIVE_PATH=SHA256): {raw}")
    if path.startswith("/"):
        raise SystemExit(f"expected file must stay inside the project: {path}")
    if len(path.encode("utf-8")) > 4096:
        raise SystemExit("expected file path exceeds the 4096-byte evidence bound")
    if any(ord(character) < 32 or ord(character) == 127 for character in path):
        raise SystemExit("expected file path contains a control character")
    if any(part in ("", ".", "..") for part in path.split("/")):
        raise SystemExit(f"expected file path has an unsafe component: {path}")
    if not re.fullmatch(r"[0-9a-fA-F]{64}", digest):
        raise SystemExit(f"expected file hash must be SHA-256: {digest}")
    if path in expected:
        raise SystemExit(f"duplicate expected file path: {path}")
    expected[path] = digest.lower()
with open(output_path, "w", encoding="utf-8") as stream:
    json.dump(expected, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY

"$git_bin" -c core.fsmonitor=false -C "$project" rev-parse HEAD \
    > "$evidence_dir/source-commit.txt"
"$git_bin" -c core.fsmonitor=false -C "$project" status --porcelain=v1 \
    > "$evidence_dir/source-status.txt"
if [[ -s "$evidence_dir/source-status.txt" ]]; then
    echo "source project must be clean for reproducible qualification" >&2
    exit 1
fi
(cd "$evidence_dir" && "$sha256sum_bin" input.json > input.sha256)
printf '%s\n' \
    '{"schema":"ryeos.remote_round_trip_evidence.v1","outbound_principal":"source_node","claim":"functional_full_project_round_trip","authenticity":"integrity-only operational transcript; workload-specific target-signed evidence is required for a stronger qualification claim"}' \
    > "$evidence_dir/authority.json"
"$python3_bin" -I -S - "$evidence_dir/source-client-authority.json" \
    "$source_app_root" "$source_daemon_url" <<'PY'
import hashlib
import json
import sys

with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(
        {
            "app_root_digest": hashlib.sha256(sys.argv[2].encode()).hexdigest(),
            "daemon_url_digest": hashlib.sha256(sys.argv[3].encode()).hexdigest(),
            "schema": "ryeos.source_client_authority.v1",
        },
        stream,
        sort_keys=True,
        separators=(",", ":"),
    )
    stream.write("\n")
PY

expected_before_hashes=()
expected_before_pairs=()
for expected in "${expected_files[@]}"; do
    relative_path="${expected%=*}"
    candidate="$project/$relative_path"
    if [[ ! -e "$candidate" && ! -L "$candidate" ]]; then
        before_hash="missing"
        expected_before_hashes+=("$before_hash")
        expected_before_pairs+=("$relative_path" "$before_hash")
        continue
    fi
    [[ ! -L "$candidate" && -f "$candidate" ]] || {
        echo "pre-existing expected output must be a regular non-symlink file: $relative_path" >&2
        exit 1
    }
    resolved_file="$("$realpath_bin" -e -- "$candidate")"
    case "$resolved_file" in
        "$project/"*) ;;
        *) echo "pre-existing expected output escapes the project: $relative_path" >&2; exit 1 ;;
    esac
    before_hash="$("$sha256sum_bin" "$resolved_file" | "$cut_bin" -d ' ' -f 1)"
    expected_before_hashes+=("$before_hash")
    expected_before_pairs+=("$relative_path" "$before_hash")
done
"$python3_bin" -I -S - "$evidence_dir/expected-files-before.json" \
    "${expected_before_pairs[@]}" <<'PY'
import json
import sys

values = sys.argv[2:]
if len(values) % 2:
    raise SystemExit("invalid expected-file before-state pairs")
before = {values[index]: values[index + 1] for index in range(0, len(values), 2)}
with open(sys.argv[1], "w", encoding="utf-8") as stream:
    json.dump(before, stream, sort_keys=True, separators=(",", ":"))
    stream.write("\n")
PY

cd -- "$evidence_dir"

run_ryeos --project "$project" remote list | "$python3_bin" -I -S -c '
import json
import sys

def redact_config_paths(value):
    if isinstance(value, dict):
        return {
            key: redact_config_paths(child)
            for key, child in value.items()
            if key != "config_path"
        }
    if isinstance(value, list):
        return [redact_config_paths(child) for child in value]
    return value

json.dump(redact_config_paths(json.load(sys.stdin)), sys.stdout,
          sort_keys=True, separators=(",", ":"))
sys.stdout.write("\n")
' > "$evidence_dir/remote-list.json"
run_ryeos --project "$project" remote status "$remote" \
    > "$evidence_dir/remote-status.json"
run_ryeos --project "$project" remote doctor "$remote" \
    > "$evidence_dir/remote-doctor.json"

"$python3_bin" -I -S - "$evidence_dir/remote-doctor.json" "$project" \
    "$remote_project" "$item_ref" "$ref_bindings_json" <<'PY'
import json
import os
import sys

doctor = json.load(open(sys.argv[1], encoding="utf-8"))
required = {
    "remote_configured",
    "remote_health",
    "remote_identity",
    "signed_authorization",
    "project_binding",
}
checks = {check.get("name"): check for check in doctor.get("checks", [])}
failed = sorted(name for name in required if checks.get(name, {}).get("ok") is not True)
if failed:
    raise SystemExit("remote doctor failed required checks: " + ", ".join(failed))
if doctor.get("remote", {}).get("health", {}).get("status") != "healthy":
    raise SystemExit("remote health endpoint did not report status=healthy")
binding = checks["project_binding"]
if os.path.realpath(binding.get("local_project_path", "")) != os.path.realpath(sys.argv[2]):
    raise SystemExit("remote doctor returned a different local project binding")
if binding.get("remote_project_path") != sys.argv[3]:
    raise SystemExit("remote doctor returned a different target project binding")
if binding.get("sync_scope") != "full_project":
    raise SystemExit("remote doctor did not confirm a full_project binding")
with open(os.path.join(os.path.dirname(sys.argv[1]), "project-binding.json"), "w", encoding="utf-8") as stream:
    json.dump(
        {
            "local_project_path": binding["local_project_path"],
            "remote_project_path": binding["remote_project_path"],
            "sync_scope": binding["sync_scope"],
        },
        stream,
        sort_keys=True,
        separators=(",", ":"),
    )
    stream.write("\n")
identity = checks["remote_identity"]
required_identity_proofs = (
    "live_identity_binding_ok",
    "configured_principal_matches",
    "configured_site_matches",
    "configured_vault_matches",
    "pinned_key_matches",
    "pinned_fingerprint_matches",
    "pinned_identity_matches",
)
missing_or_false = [
    field for field in required_identity_proofs if identity.get(field) is not True
]
if missing_or_false:
    raise SystemExit(
        "remote doctor did not prove the complete configured identity: "
        + ", ".join(missing_or_false)
    )
site_id = identity.get("site_id")
if not isinstance(site_id, str) or not site_id.startswith("site:"):
    raise SystemExit("remote doctor did not retain a canonical live site identity")
if identity.get("configured_site_id") != site_id:
    raise SystemExit("remote doctor live and configured site identities differ")
if identity.get("configured_principal_id") != identity.get("principal_id"):
    raise SystemExit("remote doctor live and configured principals differ")
if identity.get("configured_vault_fingerprint") != identity.get("vault_fingerprint"):
    raise SystemExit("remote doctor live and configured vault fingerprints differ")
ref_bindings = json.loads(sys.argv[5])
with open(os.path.join(os.path.dirname(sys.argv[1]), "probe-context.json"), "w", encoding="utf-8") as stream:
    json.dump(
        {
            "item_ref": sys.argv[4],
            "ref_bindings": ref_bindings,
            "remote_project_path": sys.argv[3],
            "target_site_id": site_id,
        },
        stream,
        sort_keys=True,
        separators=(",", ":"),
    )
    stream.write("\n")
PY

run_ryeos --no-project execute service:sync/jobs/list \
    --no-stream \
    --input '{"limit":100}' \
    > "$evidence_dir/remote-execute-jobs-before.json" \
    2> "$evidence_dir/remote-execute-jobs-before.stderr"
printf '%s\n' 0 > "$evidence_dir/remote-execute-jobs-before.exit"

execute_args=(
    --project "$project"
    remote execute "$remote" "$item_ref"
    --ref-bindings "$ref_bindings_json"
    --parameters "$input_json"
)
set +e
run_ryeos "${execute_args[@]}" \
    > "$evidence_dir/remote-execute.json" \
    2> "$evidence_dir/remote-execute.stderr"
execute_status=$?
set -e
printf '%s\n' "$execute_status" > "$evidence_dir/remote-execute.exit"

if (( execute_status != 0 )); then
    set +e
    run_ryeos --no-project execute service:sync/jobs/list \
        --no-stream \
        --input '{"limit":100}' \
        > "$evidence_dir/remote-execute-jobs-after.json" \
        2> "$evidence_dir/remote-execute-jobs-after.stderr"
    job_list_status=$?
    set -e
    printf '%s\n' "$job_list_status" > "$evidence_dir/remote-execute-jobs-after.exit"
    : > "$evidence_dir/remote-execute-new-job-candidate-ids.txt"
    if (( job_list_status == 0 )); then
        "$python3_bin" -I -S - "$evidence_dir/remote-execute-jobs-before.json" \
        "$evidence_dir/remote-execute-jobs-after.json" <<'PY' \
        > "$evidence_dir/remote-execute-new-job-candidate-ids.txt" || true
import json
import sys
import uuid

def is_remote_execute_job_id(value):
    if not isinstance(value, str) or not value.startswith("remote-execute:"):
        return False
    suffix = value.removeprefix("remote-execute:")
    try:
        return str(uuid.UUID(suffix)) == suffix
    except ValueError:
        return False

before = {
    job.get("job_id")
    for job in json.load(open(sys.argv[1], encoding="utf-8")).get("jobs", [])
    if isinstance(job.get("job_id"), str)
}
jobs = json.load(open(sys.argv[2], encoding="utf-8")).get("jobs", [])
matches = [
    job for job in jobs
    if job.get("operation_type") == "remote_execute"
    and job.get("job_id") not in before
    and is_remote_execute_job_id(job.get("job_id"))
]
for job in sorted(matches, key=lambda value: value["job_id"]):
    print(job["job_id"])
PY
    fi
    : > "$evidence_dir/remote-execute-new-job-ids.txt"
    job_index=0
    while IFS= read -r job_id; do
        [[ -n "$job_id" ]] || continue
        job_index=$((job_index + 1))
        printf '{"job_id":"%s"}\n' "$job_id" \
            > "$evidence_dir/remote-execute-new-job-$job_index-inspect-input.json"
        set +e
        run_ryeos --no-project execute service:sync/jobs/inspect \
            --no-stream \
            --input "$evidence_dir/remote-execute-new-job-$job_index-inspect-input.json" \
            > "$evidence_dir/remote-execute-new-job-$job_index-inspect.json" \
            2> "$evidence_dir/remote-execute-new-job-$job_index-inspect.stderr"
        inspect_status=$?
        set -e
        printf '%s\n' "$inspect_status" \
            > "$evidence_dir/remote-execute-new-job-$job_index-inspect.exit"
        if (( inspect_status == 0 )) && "$python3_bin" -I -S - \
            "$evidence_dir/remote-execute-new-job-$job_index-inspect.json" \
            "$evidence_dir/probe-context.json" "$job_id" <<'PY'
import json
import sys

inspection = json.load(open(sys.argv[1], encoding="utf-8"))
context = json.load(open(sys.argv[2], encoding="utf-8"))
job_id = sys.argv[3]
job = inspection.get("job", {})
operation = job.get("operation", {})
if (
    inspection.get("status") != "found"
    or job.get("job_id") != job_id
    or job.get("operation_type") != "remote_execute"
    or operation.get("item_ref") != context["item_ref"]
    or operation.get("ref_bindings") != context["ref_bindings"]
    or operation.get("target_site_id") != context["target_site_id"]
    or operation.get("remote_project_path") != context["remote_project_path"]
):
    raise SystemExit(1)
PY
        then
            printf '%s\n' "$job_id" >> "$evidence_dir/remote-execute-new-job-ids.txt"
        fi
    done < "$evidence_dir/remote-execute-new-job-candidate-ids.txt"
    matching_job_count="$("$wc_bin" -l < "$evidence_dir/remote-execute-new-job-ids.txt")"
    if (( matching_job_count > 1 )); then
        echo "multiple exact remote_execute jobs appeared; acceptance is ambiguous" >&2
    fi
    printf '%s\n' failed > "$status_file"
    (
        cd "$evidence_dir"
        "$find_bin" . -maxdepth 1 -type f ! -name failure-evidence.sha256 \
            -printf '%P\0' | "$sort_bin" -z | "$xargs_bin" -0 "$sha256sum_bin" \
            > failure-evidence.sha256
    )
    echo "remote execute returned $execute_status; acceptance may be ambiguous" >&2
    exit "$execute_status"
fi

job_id="$("$python3_bin" -I -S - "$evidence_dir/remote-execute.json" <<'PY'
import json
import sys
import uuid

execution = json.load(open(sys.argv[1], encoding="utf-8"))
job_id = execution.get("job_id")
suffix = job_id.removeprefix("remote-execute:") if isinstance(job_id, str) else ""
try:
    canonical = str(uuid.UUID(suffix)) == suffix
except ValueError:
    canonical = False
if not canonical:
    raise SystemExit("remote execute response has no canonical source job_id")
print(job_id)
PY
)"
printf '%s\n' "$job_id" > "$evidence_dir/remote-execute-job-id.txt"
printf '{"job_id":"%s"}\n' "$job_id" > "$evidence_dir/job-inspect-input.json"
run_ryeos --no-project execute service:sync/jobs/inspect \
    --no-stream \
    --input "$evidence_dir/job-inspect-input.json" \
    > "$evidence_dir/remote-execute-job-inspect.json"

"$python3_bin" -I -S - "$evidence_dir/remote-execute.json" \
    "$evidence_dir/remote-execute-job-inspect.json" "$job_id" \
    "$evidence_dir/probe-context.json" <<'PY'
import json
import sys

execution = json.load(open(sys.argv[1], encoding="utf-8"))
inspection = json.load(open(sys.argv[2], encoding="utf-8"))
job_id = sys.argv[3]
context = json.load(open(sys.argv[4], encoding="utf-8"))
push = execution.get("push", {}).get("snapshot_hash")
remote = execution.get("remote", {}).get("snapshot_hash")
pull = execution.get("pull", {}).get("snapshot_hash")
if not all(isinstance(value, str) and value for value in (push, remote, pull)):
    raise SystemExit("remote execute did not return coherent push/result/pull snapshots")
if remote != pull:
    raise SystemExit("remote result and pulled snapshot differ")
if inspection.get("status") != "found":
    raise SystemExit("remote execution job is not inspectable")
job = inspection.get("job", {})
if job.get("job_id") != job_id:
    raise SystemExit("remote execution inspection returned a different job")
if job.get("operation_type") != "remote_execute" or job.get("state") != "completed":
    raise SystemExit("remote execution job is not completed")
operation = job.get("operation", {})
for field in ("item_ref", "ref_bindings", "target_site_id", "remote_project_path"):
    if operation.get(field) != context[field]:
        raise SystemExit(f"remote execution job has a different {field}")
if push not in job.get("uploaded_hashes", []) or pull not in job.get("fetched_hashes", []):
    raise SystemExit("remote execution job hashes do not match the round trip")
attempts = inspection.get("attempts", [])
retention = inspection.get("attempt_retention", {})
if (
    retention.get("mode") != "complete"
    or retention.get("cumulative_count") != 1
    or retention.get("retained_count") != 1
    or retention.get("terminal_row_limit") is not None
):
    raise SystemExit("remote execution attempt history is not exactly and completely retained")
if (
    len(attempts) != 1
    or attempts[0].get("job_id") != job_id
    or attempts[0].get("attempt_number") != 1
    or attempts[0].get("state") != "completed"
):
    raise SystemExit("remote execution attempt is not uniquely completed")
PY

: > "$evidence_dir/observed-files.sha256"
expected_index=0
for expected in "${expected_files[@]}"; do
    relative_path="${expected%=*}"
    expected_hash="${expected##*=}"
    [[ ! -L "$project/$relative_path" && -f "$project/$relative_path" ]] || {
        echo "expected pulled output must be a regular non-symlink file: $relative_path" >&2
        exit 1
    }
    resolved_file="$("$realpath_bin" -e -- "$project/$relative_path")"
    case "$resolved_file" in
        "$project/"*) ;;
        *)
            echo "expected pulled output escapes the project: $relative_path" >&2
            exit 1
            ;;
    esac
    observed_hash="$("$sha256sum_bin" "$resolved_file" | "$cut_bin" -d ' ' -f 1)"
    [[ "${observed_hash,,}" == "${expected_hash,,}" ]] || {
        echo "unexpected hash for pulled file $relative_path" >&2
        exit 1
    }
    before_hash="${expected_before_hashes[$expected_index]}"
    expected_index=$((expected_index + 1))
    [[ "$before_hash" != "$observed_hash" ]] || {
        echo "expected output did not transition during the remote round trip: $relative_path" >&2
        exit 1
    }
    printf '%s  %s\n' "$observed_hash" "$relative_path" \
        >> "$evidence_dir/observed-files.sha256"
done

printf '%s\n' passed > "$status_file"
(
    cd "$evidence_dir"
    "$sha256sum_bin" \
        authority.json \
        status \
        input.sha256 \
        input.json \
        job-inspect-input.json \
        expected-files.json \
        expected-files-before.json \
        observed-files.sha256 \
        probe-context.json \
        project-binding.json \
        remote-doctor.json \
        remote-execute.exit \
        remote-execute.stderr \
        remote-execute-job-id.txt \
        remote-execute-job-inspect.json \
        remote-execute-jobs-before.exit \
        remote-execute-jobs-before.json \
        remote-execute-jobs-before.stderr \
        remote-execute.json \
        remote-list.json \
        remote-status.json \
        source-client-authority.json \
        source-commit.txt \
        source-status.txt \
        > evidence.sha256
)

echo "node-principal remote round-trip passed; evidence retained at $evidence_dir"
