#!/usr/bin/env bash

# Lightweight regression cases for resolve-version.sh. This script performs no
# build and is intended for CI or explicit operator execution.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RESOLVER="$ROOT/scripts/release/resolve-version.sh"

expect_version() {
    local expected="$1"
    shift

    local actual
    actual="$($RESOLVER "$@")"
    if [[ "$actual" != "$expected" ]]; then
        echo "expected version '$expected', got '$actual'" >&2
        exit 1
    fi
}

expect_rejected() {
    if "$RESOLVER" "$@" >/dev/null 2>&1; then
        echo "expected release version input to be rejected: $*" >&2
        exit 1
    fi
}

expect_version "0.5.0" push "v0.5.0" ""
expect_version "12.34.56" workflow_dispatch "main" "12.34.56"
expect_version "1.0.0-rc.1" workflow_dispatch "main" "1.0.0-rc.1"
expect_version "1.0.0-alpha-2" workflow_dispatch "main" "1.0.0-alpha-2"

expect_rejected push "main" ""
expect_rejected workflow_dispatch "main" ""
expect_rejected workflow_dispatch "main" "v1.2.3"
expect_rejected workflow_dispatch "main" "01.2.3"
expect_rejected workflow_dispatch "main" "1.02.3"
expect_rejected workflow_dispatch "main" "1.2.03"
expect_rejected workflow_dispatch "main" "1.2.3-rc.01"
expect_rejected workflow_dispatch "main" "1.2.3+build.1"
expect_rejected workflow_dispatch "main" '1.2.3"; echo injected; #'
expect_rejected workflow_dispatch "main" $'1.2.3\nforged=output'
expect_rejected workflow_dispatch "main" '$(id)'
expect_rejected workflow_dispatch "main" '1.2.3; id'

echo "release version resolver cases passed"
