#!/usr/bin/env bash
# ryeos:signed:2026-11-28T04:04:46Z:fff72d85317671d92fbc23acfb49c00bc9e341d14368a5a11926c483d68c220f:QBcFlPgUM9CUyGM3yFdezfIv3nqYT63vBgxHTX9vZ+3H/arb0u7Dtk6szqXu/lr20tlgeyfhMqsJ/I3WIC1eCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority.
#
# rm-node-capacity.sh: keep a roadmap node's native capacity topped up through
# the node's own operator commands only (G-13).
#
#   rm-node-capacity.sh <track> [--below N] [--reason TEXT] [--stop] [--start]
#     --below N    reissue only when the node has N or fewer incarnations left
#                  (default 2); above that the helper reports and exits 0
#     --reason T   the reissue's recorded reason (default "roadmap capacity keeper")
#     --stop       stop a running node first (without it a running node is refused:
#                  a reissue needs a stopped node)
#     --start      after the reissue, clear any restart inhibition, start the node
#                  and run `ryeos node doctor` (without it the node stays stopped,
#                  so an idle node spends no incarnation)
#
# The node is the one under /opt/ryeos-rm-<track> with its app root under
# /home/leo/ryeos-rm/<track>/app; any other track is refused. Each step is
# written to the track's lifecycle log.
set -euo pipefail

usage() { sed -n '/^#   rm-node-capacity.sh/,/^# so an idle/p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
[[ $# -ge 1 ]] || usage
track=$1; shift
below=2 reason="roadmap capacity keeper" stop=0 start=0
while [[ $# -gt 0 ]]; do
    case $1 in
        --below) below=${2:?}; shift 2 ;;
        --reason) reason=${2:?}; shift 2 ;;
        --stop) stop=1; shift ;;
        --start) start=1; shift ;;
        *) usage ;;
    esac
done
[[ $track =~ ^[a-z][a-z0-9-]{0,30}[a-z0-9]$ && $track != stage ]] || { echo "rm-node-capacity: bad track name: $track" >&2; exit 2; }
[[ $below =~ ^[0-9]+$ ]] || { echo "rm-node-capacity: --below must be a whole number" >&2; exit 2; }
base="/home/leo/ryeos-rm/$track"
app_root="$base/app"
log="$base/lifecycle.log"
ryeos="/opt/ryeos-rm-$track/bin/ryeos"
[[ -f $log && -x $ryeos ]] || { echo "rm-node-capacity: $track is not a node installed by rm-node-up" >&2; exit 2; }
mark() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$1" >> "$log"; echo "rm-node-capacity[$track]: $1"; }

export RYEOS_APP_ROOT="$app_root"
unset RYEOS_CLIENT_KEY || true

status="$("$ryeos" node status --app-root "$app_root" 2>&1)" || true
remaining="$(sed -n 's/^incarnations remaining: \([0-9][0-9]*\)$/\1/p' <<<"$status" | head -n1)"
[[ -n $remaining ]] || { echo "rm-node-capacity: cannot read the incarnations remaining from: $status" >&2; exit 1; }
if (( remaining > below )); then
    echo "rm-node-capacity[$track]: $remaining incarnations left (reissue at $below or fewer): nothing to do"
    exit 0
fi
if head -n1 <<<"$status" | grep -q '^running'; then
    [[ $stop == 1 ]] || { echo "rm-node-capacity: $track is running; a reissue needs it stopped (give --stop)" >&2; exit 2; }
    "$ryeos" stop --app-root "$app_root"
    mark "capacity-stopped"
fi
"$ryeos" node capacity reissue --reason "$reason" --app-root "$app_root"
mark "capacity-reissued ($remaining left before)"
if [[ $start == 1 ]]; then
    "$ryeos" node allow-restart --app-root "$app_root" || true
    "$ryeos" start --app-root "$app_root"
    mark "capacity-started"
    "$ryeos" node doctor --app-root "$app_root"
    mark "capacity-doctor"
fi
