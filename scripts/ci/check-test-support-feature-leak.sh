#!/usr/bin/env bash
# Fail if any shipped ryeosd or ryeos-cli feature set resolves a test-support
# feature (including handoff-test-support and crash-qualification-test-support)
# through normal or build dependency edges. Each package is printed with its
# resolved feature list, which also covers the root package's own features.

set -euo pipefail

check() {
    local package="$1" features="$2" tree
    local -a selection=(--no-default-features)
    if [ -n "$features" ]; then
        selection+=(--features "$features")
    fi
    tree="$(cargo tree --locked -p "$package" "${selection[@]}" \
        --edges normal,build --prefix none --format '{p} [{f}]')"
    if grep -q 'test-support' <<<"$tree"; then
        echo "test-support feature leaked into $package [${features:-<none>}]:" >&2
        grep -E 'test-support' <<<"$tree" | sort -u >&2
        return 1
    fi
    echo "ok: $package [${features:-<none>}]"
}

status=0
for features in "" scheduling model-provider-setup scheduling,model-provider-setup; do
    check ryeosd "$features" || status=1
    check ryeos-cli "$features" || status=1
done
check ryeosd latency-profiling || status=1
check ryeosd scheduling,model-provider-setup,latency-profiling || status=1
exit "$status"
