#!/usr/bin/env bash

# Run the real `ryeos init` for every node profile a source tree ships, under
# every host class that profile declares, on a distinct mount. Profiles and
# host classes are read from the signed source tree, never listed here, so a
# new profile or class is covered the day it ships.
#
# The durable-root storage gate refuses an app root on an overlay, tmpfs or the
# machine's root disk, and a package-time preflight that never ran the real
# binary on a durable mount went unnoticed until release day. This check keeps
# that gate exercised: it fails when any init is refused for storage, and
# when an init fails for any other reason that is not a host refusing a
# sandbox it cannot provide (a CI VM proves no per-job process scopes).
#
# Usage: check.sh --ryeos PATH --source DIR --trust-file FILE --verify-root DIR
#   --verify-root must be an existing directory on its own mount; every app
#   root is a fresh subdirectory that is removed afterwards.

set -euo pipefail
export LC_ALL=C
root="$(cd "$(dirname "$0")/../../.." && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$root/scripts/lib/ryeos-terminal.sh"
ryeos_term_init

usage() {
    ryeos_term_fail "usage: $0 --ryeos PATH --source DIR --trust-file FILE --verify-root DIR"
    exit 2
}

ryeos_bin=""
source_dir=""
trust_file=""
verify_root=""
while (($#)); do
    case "$1" in
        --ryeos) ryeos_bin="${2:-}"; shift 2 ;;
        --source) source_dir="${2:-}"; shift 2 ;;
        --trust-file) trust_file="${2:-}"; shift 2 ;;
        --verify-root) verify_root="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -x "$ryeos_bin" && -d "$source_dir/.ai" && -f "$trust_file" && -d "$verify_root" ]] || usage

profile_dir="$source_dir/.ai/node/init/profiles"
[[ -d "$profile_dir" ]] || {
    ryeos_term_fail "source has no node init profiles: $profile_dir"
    exit 2
}

# The mount the app roots live on must not be the root's own.
[[ "$(stat -c %m "$verify_root")" != / ]] || {
    ryeos_term_fail "verification root is on the root mount: $verify_root"
    exit 2
}

substrate_digest="sha256:$(sha256sum -- "$ryeos_bin" | awk '{print $1}')"
work="$(mktemp -d "$verify_root/init-profiles.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# One "profile host_class bundle..." line per pair, from the profile files.
pairs="$(python3 -I - "$profile_dir" <<'PY'
import sys, pathlib, yaml
for path in sorted(pathlib.Path(sys.argv[1]).glob("*.yaml")):
    doc = yaml.safe_load(path.read_text())
    classes = (((doc.get("policies") or {}).get("isolation") or {}).get("policy") or {}).get("host_classes") or {}
    for name in sorted(classes):
        print(path.stem, name, *doc.get("exact_bundles", []))
PY
)"
[[ -n "$pairs" ]] || {
    ryeos_term_fail "no profile declares a host class"
    exit 2
}

passed=0
host_refused=0
skipped=0
failed=0
n=0
while read -r profile host_class bundles; do
    n=$((n + 1))
    label="$profile/$host_class"
    missing=""
    for bundle in $bundles; do
        [[ -d "$source_dir/$bundle/.ai" ]] || missing+=" $bundle"
    done
    if [[ -n "$missing" ]]; then
        ryeos_term_info "skip $label: bundle not present in this source:$missing"
        skipped=$((skipped + 1))
        continue
    fi
    app_root="$work/node-$n"
    log="$work/init-$n.log"
    if timeout 300 "$ryeos_bin" init \
        --non-interactive \
        --app-root "$app_root" \
        --source "$source_dir" \
        --trust-file "$trust_file" \
        --substrate-image-digest "$substrate_digest" \
        --substrate-protocol 1 \
        --node-profile "$profile" \
        --host-class "$host_class" >"$log" 2>&1; then
        ryeos_term_info "ok   $label"
        passed=$((passed + 1))
    elif grep -q 'storage:durable_root' "$log"; then
        ryeos_term_fail "$label refused at the durable-root gate"
        sed 's/^/    /' "$log" >&2
        failed=$((failed + 1))
    elif grep -q 'enforced_lane_admission_refused' "$log"; then
        ryeos_term_info "host $label: the durable-root gate passed; this host cannot provide the enforced sandbox"
        host_refused=$((host_refused + 1))
    else
        ryeos_term_fail "$label failed"
        sed 's/^/    /' "$log" >&2
        failed=$((failed + 1))
    fi
    rm -rf "$app_root" "$log"
done <<<"$pairs"

ryeos_term_info "profiles x host classes: $n; initialized $passed; sandbox refused by host $host_refused; skipped $skipped; failed $failed"
((failed == 0)) || exit 1
((passed + host_refused > 0)) || {
    ryeos_term_fail "no profile was initialized"
    exit 1
}
