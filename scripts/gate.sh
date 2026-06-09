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
            [[ $# -ge 2 ]] || { echo "gate: --bundle-set requires a value" >&2; exit 2; }
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
        echo "gate: signing key not found at $KEY" >&2
        echo "gate: set KEY=/path/to/PUBLISHER.pem (or create $KEY)" >&2
        exit 2
    fi
    echo "gate: explicitly refreshing bundles (bundle-set: $bundle_set)"
    "$ROOT/scripts/populate-bundles.sh" --key "$KEY" --owner "$OWNER" --bundle-set "$bundle_set"
elif [[ "$skip_tests" == "1" ]]; then
    echo "gate: --no-tests without --refresh-bundles has nothing to do" >&2
    echo "gate: pass --refresh-bundles --no-tests for the old populate-only behavior" >&2
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

echo "gate: cargo nextest run --workspace --no-fail-fast (build_jobs=${build_jobs}, test_threads=${test_threads}) ${nextest_args[*]:-}"
RYEOS_TEST_SKIP_BUNDLE_REFRESH=1 \
    "$CARGO" nextest run --workspace --no-fail-fast \
    "${cargo_jobs_args[@]}" "${test_threads_args[@]}" "${nextest_args[@]:-}"
