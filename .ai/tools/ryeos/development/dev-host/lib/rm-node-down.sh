#!/usr/bin/env bash
# ryeos:signed:2026-11-27T06:06:44Z:71559693adff76b5940af71981498bd259f7235ebee9558a61e093a7c0eece73:6rn8helkgJI6nMhZNEi9fezqy+pEz5FnQRicxVNtNsmSFYxaIjDf/Q8L2DvzXkSE7QgVUcTVajaV/qofisYrBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority.
#
# rm-node-down.sh: stop, retire and remove one roadmap node in the only safe
# order. Removing first deletes the daemon image `host retire` needs and
# strands the node's runit service, which no sudo rule can clear.
#
#   rm-node-down.sh <track> --own-history-only [--scopes yes|no]
#
# Removal is a cut for any node that holds another principal's history
# (rulings 1.37 and 1.39): until the per-principal export exists, such a node
# is removed only as a blow-out. So this refuses unless the track's row in
# G-4's table (10-running-unattended.md) says `own-history-only`; a missing
# row is refused. `--own-history-only` is a second confirmation, never the
# only one. Once the owner column exists on a node the stored owner ids are
# read instead (not built yet).
#
# --scopes yes (the default when the lifecycle log shows a provisioned node)
# runs `host retire`; `no` skips it for a node that was never provisioned.
set -euo pipefail

usage() { sed -n '/^#   rm-node-down.sh/,/^# --scopes/p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
[[ $# -ge 2 ]] || usage
track=$1; shift
confirm=0 scopes=auto
while [[ $# -gt 0 ]]; do
    case $1 in
        --own-history-only) confirm=1; shift ;;
        --scopes) scopes=${2:?}; shift 2 ;;
        *) usage ;;
    esac
done
[[ $confirm == 1 ]] || usage
[[ $track =~ ^[a-z][a-z0-9-]{0,30}[a-z0-9]$ && $track != stage ]] || { echo "rm-node-down: bad track name: $track" >&2; exit 2; }

table="${RM_NODE_G4_TABLE:-/home/leo/projects/ryeos-next/.tmp/ryeos-full-public-20261126/10-running-unattended.md}"
row="$(grep -F "| \`$track\` |" "$table" | head -1 || true)"
[[ -n $row ]] || { echo "rm-node-down: refusing: no row for track $track in G-4's table ($table)" >&2; exit 3; }
[[ $row == *own-history-only* ]] || { echo "rm-node-down: refusing: the G-4 row for $track does not say own-history-only; removal needs the per-principal export first" >&2; exit 3; }

prefix="/opt/ryeos-rm-$track"
base="/home/leo/ryeos-rm/$track"
app_root="$base/app"
log="$base/lifecycle.log"
ryeos="$prefix/bin/ryeos"
[[ -x $ryeos ]] || { echo "rm-node-down: $ryeos is missing: nothing installed, or already removed" >&2; exit 2; }
mark() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$1" >> "$log"; echo "rm-node-down[$track]: $1"; }

export RYEOS_APP_ROOT="$app_root"
unset RYEOS_CLIENT_KEY || true
if [[ $scopes == auto ]]; then
    if grep -q ' provisioned$' "$log" 2>/dev/null; then scopes=yes; else scopes=no; fi
fi

"$ryeos" stop --app-root "$app_root" </dev/null || echo "rm-node-down[$track]: stop reported a failure (already stopped?); continuing only if the node is down" >&2
if "$ryeos" node status --app-root "$app_root" </dev/null 2>&1 | grep -qiE 'running|ready|started'; then
    echo "rm-node-down: the node still looks up; not retiring or removing" >&2
    exit 1
fi
mark stopped
if [[ $scopes == yes ]]; then
    timeout 120 "$ryeos" node host retire --confirm --app-root "$app_root" </dev/null
    mark retired
fi
sudo -n /usr/local/sbin/ryeos-rm-node remove "$track" </dev/null
mark removed
