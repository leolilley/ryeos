#!/usr/bin/env bash
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
# Usage: apt-update.sh   (run on the runner; uses sudo unless already root)
set -euo pipefail

sudo=()
if [[ "$(id -u)" != 0 ]]; then
    sudo=(sudo -n)
fi

dir=/etc/apt/sources.list.d
if [[ -d "$dir" ]]; then
    while IFS= read -r -d '' file; do
        "${sudo[@]}" rm -f -- "$file"
    done < <(grep -rlZ 'packages\.microsoft\.com' "$dir" 2>/dev/null || true)
fi
if grep -q 'packages\.microsoft\.com' /etc/apt/sources.list 2>/dev/null; then
    "${sudo[@]}" sed -i '/packages\.microsoft\.com/d' /etc/apt/sources.list
fi

attempts=4
for attempt in $(seq 1 "$attempts"); do
    if "${sudo[@]}" apt-get update; then
        exit 0
    fi
    if [[ "$attempt" -lt "$attempts" ]]; then
        echo "apt-get update failed (attempt $attempt of $attempts); retrying" >&2
        sleep $((attempt * 10))
    fi
done
echo "apt-get update failed after $attempts attempts" >&2
exit 1
