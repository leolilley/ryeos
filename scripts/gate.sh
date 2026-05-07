#!/usr/bin/env bash
# Workspace gate: builds + installs bundle binaries + rebuilds CAS manifests
# (via populate-bundles.sh), then runs the full nextest workspace gate.
#
# Bundle bin/ contents and CAS manifests are .gitignored derivable artifacts.
# This script regenerates them on every run so tests have a fresh, consistent
# bundle tree to read.
#
# Usage:
#     ./scripts/gate.sh                 # full workspace
#     ./scripts/gate.sh -p ryeosd       # forwarded to nextest
#     ./scripts/gate.sh --no-tests      # only populate bundles, skip tests
#
# This is the canonical gate; CI and humans should both invoke it.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-/home/leo/.local/share/cargo/bin/cargo}"

skip_tests=0
nextest_args=()
for arg in "$@"; do
    case "$arg" in
        --no-tests) skip_tests=1 ;;
        --no-bundle) ;;  # accepted for back-compat, no-op (drift check removed)
        *) nextest_args+=("$arg") ;;
    esac
done

echo "gate: populating bundles (build + install + rebuild-manifest)"
"$ROOT/scripts/populate-bundles.sh"

if [[ "$skip_tests" == "1" ]]; then
    exit 0
fi

echo "gate: cargo nextest run --workspace --no-fail-fast ${nextest_args[*]:-}"
"$CARGO" nextest run --workspace --no-fail-fast "${nextest_args[@]:-}"
