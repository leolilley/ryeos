#!/bin/sh
# Lay out /ryeos exactly as a sandbox activation does, then run the rehearsal
# host, which starts the owner as an activation would, answers its attach
# request as a controller would (when the plan carries that authority) and
# reports what the supervisor and launcher did afterwards. Run as root from
# any directory; every input is beside this script. stdout carries a labelled
# report whose JSON outcome follows the line "-- outcome"; the exit status is
# the host's.
set -u
cd "$(dirname "$0")" || exit 2
[ "$(id -u)" = 0 ] || { echo "run.sh must run as root" >&2; exit 2; }
HERE=$(pwd)

echo "-- host facts"
uname -srm
id
grep -E '^(MemTotal|MemAvailable|SwapTotal)' /proc/meminfo 2>&1
nproc 2>&1
ulimit -a 2>&1
for f in memory.max memory.high memory.swap.max memory.events pids.max pids.current cpu.max; do
    [ -r "/sys/fs/cgroup/$f" ] && printf '%s: %s\n' "$f" "$(tr '\n' ' ' < "/sys/fs/cgroup/$f")"
done
df -h /ryeos /tmp /dev/shm 2>&1
mount 2>&1 | grep -E ' on (/|/ryeos|/tmp|/proc|/dev/shm) ' | head -8

set -e
rm -rf /ryeos/guest-runtime /ryeos/activation /ryeos/occurrences
mkdir -p /ryeos/guest-runtime /ryeos/activation
tar -C /ryeos/guest-runtime -xpf guest-runtime.tar
# The tar carries these owners and modes; restate them for any tar.
chown -R root:root /ryeos/guest-runtime
chmod 0755 /ryeos/guest-runtime/bin /ryeos/guest-runtime/bin/ryeos-external-guest-occurrence-owner
chmod 0644 /ryeos/guest-runtime/controller-root.hex /ryeos/guest-runtime/guest-owner-profile.json
chown root:root /ryeos /ryeos/activation
chmod 0750 /ryeos /ryeos/guest-runtime /ryeos/activation
for name in signed-import.json guest-package; do
    cp "$name" "/ryeos/activation/$name"
    chown root:root "/ryeos/activation/$name"
    chmod 0600 "/ryeos/activation/$name"
done
set +e

# Sample the guest processes while the host runs: their state, what they wait
# on and which files they hold, one block per second.
SAMPLES=$HERE/process-samples.txt
: > "$SAMPLES"
(
    while :; do
        printf '== %s\n' "$(date +%T)" >> "$SAMPLES"
        for proc in /proc/[0-9]*; do
            pid=${proc#/proc/}
            args=$(tr '\0' ' ' < "$proc/cmdline" 2>/dev/null | cut -c1-110) || continue
            case $args in
                */ryeos/*|*ryeos-*|*rehearsal-host*) ;;
                *) continue ;;
            esac
            [ "$pid" = "$$" ] && continue
            state=$(sed -n 's/^State:[[:space:]]*//p' "$proc/status" 2>/dev/null | head -1)
            ppid=$(sed -n 's/^PPid:[[:space:]]*//p' "$proc/status" 2>/dev/null | head -1)
            printf '%s ppid=%s [%s] wchan=%s fds=%s %s\n' "$pid" "$ppid" "$state" \
                "$(cat "$proc/wchan" 2>/dev/null)" "$(ls "$proc/fd" 2>/dev/null | wc -l)" "$args" >> "$SAMPLES"
        done
        sleep 1
    done
) &
SAMPLER=$!

RYEOS_GUEST_START_REHEARSAL_ON_HOST="$HERE" ./rehearsal-host > "$HERE/outcome.json" 2> "$HERE/host.stderr"
RC=$?
kill "$SAMPLER" 2>/dev/null
wait "$SAMPLER" 2>/dev/null

echo "-- host exit"
echo "$RC"
echo "-- host stderr"
tail -c 4000 "$HERE/host.stderr"
echo "-- occurrence tree"
find /ryeos/occurrences -maxdepth 4 -printf '%M %u:%g %s %TT %p\n' 2>&1 | head -60
echo "-- guest process states seen (pid ppid state wait-channel command)"
grep -v '^==' "$SAMPLES" | sed 's/ fds=[0-9]*//' | cut -c1-170 | sort -n -u | head -80
echo "-- last samples"
tail -n 12 "$SAMPLES" | cut -c1-170
echo "-- outcome"
cat "$HERE/outcome.json"
echo
exit "$RC"
