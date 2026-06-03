#!/usr/bin/env bash
set -euo pipefail

declare -a forbidden=(
    'content_wrap'
    'first.?cut'
    'v0\.3\.0-first'
    'old fallback'
    'BACKCOMPAT'
    'compat shim'
)

fail=0
for pat in "${forbidden[@]}"; do
    hits=$(rg -n -i "$pat" \
        bundles/ crates/engine/ryeos-runtime/src crates/runtimes/directive/src docs/ \
        --glob '!target/**' 2>/dev/null || true)
    if [[ -n "$hits" ]]; then
        echo "ERROR: forbidden term '$pat' found:"
        echo "$hits"
        echo
        fail=1
    fi
done

exit $fail
