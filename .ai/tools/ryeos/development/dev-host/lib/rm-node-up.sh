#!/usr/bin/env bash
# ryeos:signed:2026-11-27T06:06:44Z:3eb968d9e567a77ebbf4b23434c471a602106bd20e53e189cfcfc5b0dbff6962:IJb2sCQMWi+OfmFJjrugEly/s6Ssb+EchxpLvQEJhIvR3Iz6yILapyuf9gukNnlgzXdqML/WrpdK/H7cRZ/QBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority.
#
# rm-node-up.sh: take one `Remote node build` CI run to a started roadmap node
# with a clean `ryeos node doctor`, through the restricted sudoer only.
#
#   rm-node-up.sh <track> <profile> <host-class> <run-id> <bind> [options]
#     --memory-bytes N   enclosing memory ceiling for `host setup` (default 1073741824)
#     --processes M      task ceiling for `host setup` (default 512)
#     --scopes yes|no    whether the host class requires process scopes, so one root
#                        `host setup` runs (required; G-4's table says which)
#     --bundle-trust     also pass each staged bundle's PUBLISHER_TRUST.toml that
#                        names the development key's fingerprint and nothing else
#
# Steps, each skipped when the node's lifecycle log already records it, so a
# restart resumes: staged, installed, initialised, provisioned (only when the
# class needs process scopes), started, doctor. The stage id is `r<run-id>`.
# The lifecycle log is /home/leo/ryeos-rm/<track>/lifecycle.log.
#
# The environment hazard: `ryeos` resolves its node from $RYEOS_APP_ROOT and
# the user's own data directory when no root is given. This helper exports
# RYEOS_APP_ROOT to the node's app root and passes --app-root to every command
# that takes it. It refuses any app root outside /home/leo/ryeos-rm/<track>/app
# and any bind that is not on 127.0.0.1. Never run it for a build that did not
# come from `fetch-ci-build.sh` with a recorded run id.
set -euo pipefail

usage() { sed -n '/^#   rm-node-up.sh/,/^# class needs/p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
[[ $# -ge 5 ]] || usage
track=$1 profile=$2 class=$3 run=$4 bind=$5; shift 5
memory=1073741824 processes=512 bundle_trust=0 scopes=
while [[ $# -gt 0 ]]; do
    case $1 in
        --memory-bytes) memory=${2:?}; shift 2 ;;
        --processes) processes=${2:?}; shift 2 ;;
        --scopes) scopes=${2:?}; shift 2 ;;
        --bundle-trust) bundle_trust=1; shift ;;
        *) usage ;;
    esac
done

[[ $track =~ ^[a-z][a-z0-9-]{0,30}[a-z0-9]$ && $track != stage ]] || { echo "rm-node-up: bad track name: $track" >&2; exit 2; }
[[ $profile =~ ^[a-z][a-z0-9-]*$ && $class =~ ^[a-z][a-z0-9_-]*$ ]] || { echo "rm-node-up: bad profile or class" >&2; exit 2; }
[[ $run =~ ^[0-9]+$ ]] || { echo "rm-node-up: the run id must be a number" >&2; exit 2; }
[[ $bind =~ ^127\.0\.0\.1:[0-9]{1,5}$ ]] || { echo "rm-node-up: the bind must be 127.0.0.1:<port>, got $bind" >&2; exit 2; }
[[ $memory =~ ^[1-9][0-9]*$ && $processes =~ ^[1-9][0-9]*$ ]] || { echo "rm-node-up: ceilings must be positive integers" >&2; exit 2; }
[[ $scopes == yes || $scopes == no ]] || { echo "rm-node-up: --scopes yes|no is required: say whether the class's process scopes are required (G-4's table)" >&2; exit 2; }
(( memory <= 1073741824 && processes <= 512 )) || { echo "rm-node-up: ceilings above 1 GiB / 512 processes are refused; the machine is shared" >&2; exit 2; }

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../../../../.." && pwd)"
stage_id="r$run"
prefix="/opt/ryeos-rm-$track"
base="/home/leo/ryeos-rm/$track"
app_root="$base/app"
log="$base/lifecycle.log"
ryeos="$prefix/bin/ryeos"
fetch="${RM_NODE_FETCH:-/home/leo/projects/ryeos-next/.tmp/ryeos-full-public-20261126/sudoer/fetch-ci-build.sh}"
[[ $app_root == "/home/leo/ryeos-rm/$track/app" ]] || { echo "rm-node-up: unexpected app root" >&2; exit 2; }

mkdir -p "$base"
chmod 0755 "$base"
touch "$log"
done_step() { grep -q " $1\$" "$log"; }
mark() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$1" >> "$log"; echo "rm-node-up[$track]: $1"; }

# Every ryeos call sees this node and no other.
export RYEOS_APP_ROOT="$app_root"
unset RYEOS_CLIENT_KEY || true

if ! done_step staged && ! done_step installed; then
    "$fetch" "$run" "$stage_id"
    mark staged
fi
if ! done_step installed; then
    sudo -n /usr/local/sbin/ryeos-rm-node install "$track" "$stage_id" </dev/null
    rm -rf "/var/tmp/ryeos-rm/stage/$stage_id"
    mark installed
fi
[[ -x $ryeos ]] || { echo "rm-node-up: $ryeos is missing after install" >&2; exit 1; }

if ! done_step initialised; then
    dev_trust="$root/.dev-keys/PUBLISHER_DEV_TRUST.toml"
    [[ -f $dev_trust ]] || { echo "rm-node-up: $dev_trust is missing" >&2; exit 1; }
    dev_fp="$(python3 -I - "$dev_trust" <<'PY'
import sys, tomllib
print(tomllib.load(open(sys.argv[1], "rb"))["fingerprint"])
PY
)"
    trust_args=(--trust-file "$dev_trust")
    if [[ $bundle_trust == 1 ]]; then
        while IFS= read -r f; do
            fp_ok="$(python3 -I - "$f" "$dev_fp" <<'PY'
import sys, tomllib
doc = tomllib.load(open(sys.argv[1], "rb"))
print("yes" if doc.get("fingerprint") == sys.argv[2] and set(doc) <= {"fingerprint", "owner"} else "no")
PY
)"
            if [[ $fp_ok == yes ]]; then
                trust_args+=(--trust-file "$f")
                printf '%s trust-file %s sha256 %s\n' "$(date -u +%FT%TZ)" "$f" "$(sha256sum "$f" | cut -d' ' -f1)" >> "$log"
            else
                echo "rm-node-up: not passing $f (it names more than the development key)" >&2
            fi
        done < <(find "$prefix/share/ryeos" -name PUBLISHER_TRUST.toml | sort)
    fi
    printf '%s trust-file %s sha256 %s\n' "$(date -u +%FT%TZ)" "$dev_trust" "$(sha256sum "$dev_trust" | cut -d' ' -f1)" >> "$log"
    "$ryeos" init --non-interactive \
        --source "$prefix/share/ryeos" \
        "${trust_args[@]}" \
        --node-profile "$profile" --host-class "$class" \
        --bind "$bind" --app-root "$app_root" </dev/null
    mark initialised
fi

# A class whose process scopes are required needs one root `host setup`.
if ! done_step provisioned; then
    if [[ $scopes == yes ]]; then
        # stdin closed and a timeout, so a sudo rule that does not match fails
        # at once with sudo's own message instead of waiting on a password.
        timeout 120 "$ryeos" node host setup --confirm --app-root "$app_root" \
            --node-memory-bytes "$memory" --node-processes "$processes" </dev/null
    fi
    mark provisioned
fi

if ! done_step started; then
    "$ryeos" start --app-root "$app_root" </dev/null
    mark started
fi
"$ryeos" node doctor --app-root "$app_root" </dev/null | tee -a "$base/doctor.log"
mark doctor
