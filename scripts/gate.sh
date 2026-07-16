#!/usr/bin/env bash
# Workspace test gate.
#
# This intentionally does NOT rebuild/repopulate bundles by default. Bundle
# refresh is expensive and should be an explicit authoring/release action.
#
# Usage:
#     ./scripts/gate.sh                         # full workspace tests
#     ./scripts/gate.sh -p ryeosd               # forwarded to nextest
#     ./scripts/gate.sh --refresh-bundles       # explicit full bundle refresh, then tests
#     ./scripts/gate.sh --refresh-bundles --no-tests
#     ./scripts/gate.sh --bundle-set hosted-node --refresh-bundles
#
# CI/release jobs that need regenerated bundle binaries/manifests must pass
# --refresh-bundles explicitly. Local UI/dev loops should not.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
gate_tty=0
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$ROOT/scripts/lib/ryeos-terminal.sh"
ryeos_term_init
if ryeos_term_is_tty; then
    gate_tty=1
fi

gate_info() {
    if [[ "$gate_tty" == 1 ]]; then ryeos_term_info "$*"; else printf 'gate: %s\n' "$*"; fi
}

gate_fail() {
    if [[ "$gate_tty" == 1 ]]; then ryeos_term_fail "$*"; else printf 'gate: %s\n' "$*" >&2; fi
}

# Default publisher signing key + owner — used by populate-bundles.sh.
# Override with KEY=... OWNER=... if you have a different setup.
KEY="${KEY:-$ROOT/.dev-keys/PUBLISHER_DEV.pem}"
OWNER="${OWNER:-ryeos-dev}"

skip_tests=0
refresh_bundles=0
bundle_set="full"
nextest_args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-tests)
            skip_tests=1
            shift
            ;;
        --refresh-bundles)
            refresh_bundles=1
            shift
            ;;
        --bundle-set)
            [[ $# -ge 2 ]] || { gate_fail "--bundle-set requires a value"; exit 2; }
            bundle_set="$2"
            shift 2
            ;;
        *)
            nextest_args+=("$1")
            shift
            ;;
    esac
done

if [[ "$refresh_bundles" == "1" ]]; then
    if [[ ! -s "$KEY" ]]; then
        gate_fail "signing key not found at $KEY"
        gate_fail "set KEY=/path/to/PUBLISHER.pem (or create $KEY)"
        exit 2
    fi
    gate_info "explicitly refreshing bundles (bundle-set: $bundle_set)"
    # --all: gate is a deliberate full bundle-set refresh; populate-bundles.sh
    # refuses an implicit full build without it.
    "$ROOT/scripts/populate-bundles.sh" --key "$KEY" --owner "$OWNER" --bundle-set "$bundle_set" --all
elif [[ "$skip_tests" == "1" ]]; then
    gate_fail "--no-tests without --refresh-bundles has nothing to do"
    gate_fail "pass --refresh-bundles --no-tests for the old populate-only behavior"
    exit 2
fi

if [[ "$skip_tests" == "1" ]]; then
    exit 0
fi

# Resource caps. The workspace has heavy integration tests (some spawn daemons),
# so running them at full parallelism can exhaust memory and lock up the machine.
# Default to half the available cores for both compilation and test execution.
# Override with GATE_TEST_THREADS / GATE_BUILD_JOBS, or set either to 0 to let
# cargo/nextest use their own defaults.
default_jobs="$(( $(nproc 2>/dev/null || echo 2) / 2 ))"
(( default_jobs < 1 )) && default_jobs=1
test_threads="${GATE_TEST_THREADS:-$default_jobs}"
build_jobs="${GATE_BUILD_JOBS:-$default_jobs}"

cargo_jobs_args=()
[[ "$build_jobs" != "0" ]] && cargo_jobs_args=(--build-jobs "$build_jobs")
test_threads_args=()
[[ "$test_threads" != "0" ]] && test_threads_args=(--test-threads "$test_threads")

gate_info "cargo nextest run --workspace --no-fail-fast (build_jobs=${build_jobs}, test_threads=${test_threads}) ${nextest_args[*]:-}"
"$CARGO" nextest run --workspace --no-fail-fast \
    "${cargo_jobs_args[@]}" "${test_threads_args[@]}" "${nextest_args[@]:-}"
