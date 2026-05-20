#!/usr/bin/env bash
set -euo pipefail

declare -a forbidden=(
    'content_wrap'
    'first.?cut'
    'v0\.3\.0-first'
    'Legacy fallback'
    'BACKCOMPAT'
    'backwards.?compat'
)

fail=0
for pat in "${forbidden[@]}"; do
    hits=$(rg -n -i "$pat" \
        bundles/ crates/core/runtime/src crates/runtimes/directive/src docs/ \
        --glob '!target/**' 2>/dev/null || true)
    if [[ -n "$hits" ]]; then
        echo "ERROR: forbidden legacy term '$pat' found:"
        echo "$hits"
        echo
        fail=1
    fi
done

exit $fail
