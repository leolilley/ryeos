#!/usr/bin/env bash
# ryeos:signed:2026-11-27T05:59:16Z:f92fdfb1ea4edf6a42a66afb27a660986a892a2536b717ed31c917497d5bdc88:1Tyg9SV1sRsJTWD7nkSzhN7yyJH9rlM9saJqd/1eTfLx7pZABreSHaH+DGsqMsyTfu+VW+zTlB3VyIFpCnVaDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked CI runner helper in the development bundle; lib/ is
# excluded from Tool resolution, so this is signed source, not an admitted
# Tool. It grants no RyeOS authority.
#
# Test of apt-update.sh. It points the helper at a temporary sources
# directory and a stub apt-get through the helper's override variables, so it
# never reads or changes the machine's /etc/apt and never runs the real
# apt-get. It shows:
#   - a packages.microsoft.com source file and a sources.list line are removed
#     and other sources are kept;
#   - an apt-get that always exits 100 makes the helper exit 1 after exactly
#     four attempts, with its failure message;
#   - an apt-get that fails twice and then succeeds makes the helper exit 0 on
#     the third attempt;
#   - an apt-get that succeeds makes the helper exit 0 after one attempt.
#
# Usage: .ai/tools/ryeos/development/runner-host/lib/test-apt-update.sh
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
helper="$here/apt-update.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() { echo "test-apt-update: FAIL: $*" >&2; exit 1; }

# A stub apt-get: counts calls in $STUB_COUNT and exits 100 until the call
# number reaches $STUB_SUCCEED_AT (0 means never).
cat > "$work/apt-get" <<'STUB'
#!/usr/bin/env bash
n=$(( $(cat "$STUB_COUNT" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$STUB_COUNT"
[[ "$1" == update ]] || exit 64
if [[ "$STUB_SUCCEED_AT" != 0 && "$n" -ge "$STUB_SUCCEED_AT" ]]; then exit 0; fi
echo "stub apt-get: 403 Forbidden" >&2
exit 100
STUB
chmod 0755 "$work/apt-get"

# run_helper <succeed-at>: fresh sources and counter, then the helper.
run_helper() {
    rm -rf "$work/sources.list.d" "$work/sources.list" "$work/count"
    mkdir -p "$work/sources.list.d"
    printf 'deb https://packages.microsoft.com/ubuntu/24.04/prod noble main\n' > "$work/sources.list.d/microsoft-prod.list"
    printf 'deb http://archive.ubuntu.com/ubuntu noble main\n' > "$work/sources.list.d/ubuntu.list"
    printf 'deb https://packages.microsoft.com/repos/x stable main\ndeb http://archive.ubuntu.com/ubuntu noble universe\n' > "$work/sources.list"
    set +e
    STUB_COUNT="$work/count" STUB_SUCCEED_AT="$1" \
    APT_UPDATE_SOURCES_DIR="$work/sources.list.d" \
    APT_UPDATE_SOURCES_LIST="$work/sources.list" \
    APT_UPDATE_APT_GET="$work/apt-get" \
    APT_UPDATE_RETRY_SECONDS=0 \
    APT_UPDATE_SUDO= \
        bash "$helper" > "$work/out" 2>&1
    status=$?
    set -e
    attempts=$(cat "$work/count" 2>/dev/null || echo 0)
}

# 1. Always failing: exit 1 after exactly four attempts, with the message.
run_helper 0
[[ "$status" == 1 ]] || fail "always-failing apt-get: expected exit 1, got $status"
[[ "$attempts" == 4 ]] || fail "always-failing apt-get: expected 4 attempts, got $attempts"
grep -q 'apt-get update failed after 4 attempts' "$work/out" || fail "failure message missing"
echo "ok: an always-failing apt-get exits 1 after 4 attempts"

# 2. The removal path ran in the same run: Microsoft sources are gone, others stay.
[[ ! -e "$work/sources.list.d/microsoft-prod.list" ]] || fail "packages.microsoft.com source file was not removed"
[[ -e "$work/sources.list.d/ubuntu.list" ]] || fail "an unrelated source file was removed"
! grep -q 'packages\.microsoft\.com' "$work/sources.list" || fail "packages.microsoft.com line left in sources.list"
grep -q 'archive.ubuntu.com/ubuntu noble universe' "$work/sources.list" || fail "an unrelated sources.list line was removed"
echo "ok: the Microsoft source file and line are removed, the others kept"

# 3. Fails twice, then succeeds: exit 0 on the third attempt.
run_helper 3
[[ "$status" == 0 ]] || fail "third-try success: expected exit 0, got $status"
[[ "$attempts" == 3 ]] || fail "third-try success: expected 3 attempts, got $attempts"
echo "ok: an apt-get that succeeds on the third try ends the helper with exit 0"

# 4. Succeeds at once: one attempt.
run_helper 1
[[ "$status" == 0 && "$attempts" == 1 ]] || fail "first-try success: exit $status after $attempts attempts"
echo "ok: an apt-get that succeeds at once is called once"

# 5. The defaults are what a runner gets: no override set means the real paths.
for pair in 'sources_dir=${APT_UPDATE_SOURCES_DIR:-/etc/apt/sources.list.d}' \
            'sources_list=${APT_UPDATE_SOURCES_LIST:-/etc/apt/sources.list}' \
            'apt_get=${APT_UPDATE_APT_GET:-apt-get}' \
            'retry_seconds=${APT_UPDATE_RETRY_SECONDS:-10}'; do
    grep -qF "$pair" "$helper" || fail "default changed: $pair"
done
echo "ok: the defaults are the runner's own paths, apt-get and a 10 second step"
