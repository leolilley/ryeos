#!/usr/bin/env bash
# ryeos:signed:2026-11-27T05:59:16Z:be1a7d4d20168f606a151c028e1b1e888332437e6f69a8e37aa51d61698a46ae:y9B4FuznAbisx78s0c2DOfjiE6RgzmGOimHfStLExcyBL8cifmY5NIV5ljFL0CCiG7X5zhbjR+b7oXtH2PR2Ag==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked CI runner helper in the development bundle; lib/ is
# excluded from Tool resolution, so this is signed source, not an admitted
# Tool. It grants no RyeOS authority.
#
# Refresh the apt package lists on a hosted runner without depending on the
# runner image's third-party sources.
#
# The Ubuntu runner image ships a packages.microsoft.com source list that we
# never install from. When that host answers 403 or is down, a plain
# `apt-get update` exits 100 and fails a job that has nothing to do with it.
# This removes those source entries first, then retries the update a few times
# for ordinary network flakes. Ubuntu's own archives are still required: if the
# update keeps failing, this fails.
#
# Usage: .ai/tools/ryeos/development/runner-host/lib/apt-update.sh   (run on the runner or as root in a container; uses sudo unless already root)
#
# Overrides, used only by test-apt-update.sh so the failure and removal paths
# run without touching the machine's apt. A runner sets none of them and gets
# the values below.
#   APT_UPDATE_SOURCES_DIR     default /etc/apt/sources.list.d
#   APT_UPDATE_SOURCES_LIST    default /etc/apt/sources.list
#   APT_UPDATE_APT_GET         default apt-get
#   APT_UPDATE_RETRY_SECONDS   seconds slept per attempt number, default 10
#   APT_UPDATE_SUDO            privilege prefix when not root, default "sudo -n"; empty for none
set -euo pipefail

sources_dir=${APT_UPDATE_SOURCES_DIR:-/etc/apt/sources.list.d}
sources_list=${APT_UPDATE_SOURCES_LIST:-/etc/apt/sources.list}
apt_get=${APT_UPDATE_APT_GET:-apt-get}
retry_seconds=${APT_UPDATE_RETRY_SECONDS:-10}

sudo=()
if [[ "$(id -u)" != 0 ]]; then
    read -r -a sudo <<<"${APT_UPDATE_SUDO-sudo -n}"
fi

dir=$sources_dir
if [[ -d "$dir" ]]; then
    while IFS= read -r -d '' file; do
        "${sudo[@]}" rm -f -- "$file"
    done < <(grep -rlZ 'packages\.microsoft\.com' "$dir" 2>/dev/null || true)
fi
if grep -q 'packages\.microsoft\.com' "$sources_list" 2>/dev/null; then
    "${sudo[@]}" sed -i '/packages\.microsoft\.com/d' "$sources_list"
fi

attempts=4
for attempt in $(seq 1 "$attempts"); do
    if "${sudo[@]}" "$apt_get" update; then
        exit 0
    fi
    if [[ "$attempt" -lt "$attempts" ]]; then
        echo "apt-get update failed (attempt $attempt of $attempts); retrying" >&2
        sleep $((attempt * retry_seconds))
    fi
done
echo "apt-get update failed after $attempts attempts" >&2
exit 1
