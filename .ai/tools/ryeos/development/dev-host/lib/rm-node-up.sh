#!/usr/bin/env bash
# ryeos:signed:2026-11-27T06:43:27Z:e2dc6a5ffe5b19d9cec4f0a8931d84615ddfaaff5270fff0dd54d3d8dc0f981e:ExD8z+mXCzNHdIjag5ZmAeipXGLTSGYG41yL/Phl7xkCE1Pk/02CVPo+soiBMnhAQmQ9MCViuL6lYfXJszaIDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
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
# A track is installed fresh each time (decision 8): no state, keys or log from an
# earlier life of the track may be picked up. rm-node-down archives the log and
# deletes the app root, so either one still present here was not made by a clean
# down and is refused rather than resumed as a new install.
if [[ ! -e $log ]]; then
    if [[ -e $app_root ]] && [[ -n "$(ls -A "$app_root" 2>/dev/null)" ]]; then
        echo "rm-node-up: $app_root is not empty and this track has no lifecycle log: it was not made by this helper; remove it by hand if it is yours" >&2
        exit 2
    fi
elif grep -q ' removed$' "$log"; then
    echo "rm-node-up: the lifecycle log of $track records a removal; rm-node-down archives it, so this is not a clean track" >&2
    exit 2
fi
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
    build_commit="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["commit"])' "$prefix/BUILD-SOURCE.json")"
    [[ $build_commit =~ ^[0-9a-f]{7,64}$ ]] || { echo "rm-node-up: bad commit in $prefix/BUILD-SOURCE.json" >&2; exit 1; }
    built_hash="$(git -C "$root" show "$build_commit:.dev-keys/PUBLISHER_DEV_TRUST.toml" | sha256sum | cut -d' ' -f1)"
    [[ $built_hash == "$(sha256sum "$dev_trust" | cut -d' ' -f1)" ]] || { echo "rm-node-up: the dev trust file in this checkout differs from the one in the build's commit $build_commit" >&2; exit 1; }
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
    # A fresh node binds to the substrate it runs on. For a native install that is
    # the same measurement scripts/pkg/install-local-direct.sh makes: sha256 over
    # the protocol line and the sha256 of each required binary, protocol 1.
    substrate_digest="$({
        printf '%s\n' 'ryeos/native-substrate/v1'
        for binary in ryeosd ryeos; do
            [[ -f "$prefix/bin/$binary" && ! -L "$prefix/bin/$binary" ]] || { echo "rm-node-up: $prefix/bin/$binary is missing" >&2; exit 1; }
            printf '%s %s\n' "$binary" "$(sha256sum -- "$prefix/bin/$binary" | cut -d' ' -f1)"
        done
    } | sha256sum | cut -d' ' -f1)"
    printf '%s substrate-digest sha256:%s protocol 1\n' "$(date -u +%FT%TZ)" "$substrate_digest" >> "$log"
    "$ryeos" init --non-interactive \
        --source "$prefix/share/ryeos" \
        "${trust_args[@]}" \
        --substrate-image-digest "sha256:$substrate_digest" --substrate-protocol 1 \
        --node-profile "$profile" --host-class "$class" \
        --bind "$bind" --app-root "$app_root" </dev/null
    mark initialised
fi

# A class whose process scopes are required needs one root `host setup`.
if ! done_step provisioned && ! done_step provision-skipped; then
    if [[ $scopes == yes ]]; then
        # stdin closed and a timeout, so a sudo rule that does not match fails
        # at once with sudo's own message instead of waiting on a password.
        timeout 120 "$ryeos" node host setup --confirm --app-root "$app_root" \
            --node-memory-bytes "$memory" --node-processes "$processes" </dev/null
        mark provisioned
    else
        mark provision-skipped
    fi
fi

# Start by the node's actual state, not the log: a resumed run may find the node stopped.
status_first_line="$("$ryeos" node status --app-root "$app_root" </dev/null 2>&1 | head -1 || true)"
if [[ $status_first_line == "initialized, stopped"* ]]; then
    "$ryeos" start --app-root "$app_root" </dev/null
    mark started
fi
port="${bind##*:}"
for _ in 1 2 3 4 5 6 7 8 9 10; do
    ss -ltnH "sport = :$port" 2>/dev/null | grep -q "$bind" && break
    sleep 1
done
ss -ltnH "sport = :$port" 2>/dev/null | grep -q "$bind" || { echo "rm-node-up: nothing is listening on $bind after start" >&2; exit 1; }
"$ryeos" node doctor --app-root "$app_root" </dev/null | tee -a "$base/doctor.log"
mark doctor
