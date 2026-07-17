#!/usr/bin/env bash

# Print the official RyeOS publisher fingerprint compiled into `ryeos init`.
# Release scripts use this helper so their trust checks cannot silently drift
# from the key the installed binary treats as authoritative.

set -euo pipefail
export LC_ALL=C

[[ $# -eq 0 ]] || {
    echo "usage: $0" >&2
    exit 2
}

root="$(cd "$(dirname "$0")/../.." && pwd)"
source_file="$root/crates/daemon/ryeos-node/src/init.rs"

fingerprints="$(
    sed -n '/pub const OFFICIAL_PUBLISHER_FP: &str/,/;/p' "$source_file" \
        | grep -oE '[0-9a-f]{64}' \
        || true
)"

if [[ "$(printf '%s\n' "$fingerprints" | sed '/^$/d' | wc -l)" -ne 1 ]]; then
    echo "official publisher fingerprint: expected exactly one 64-hex value in $source_file" >&2
    exit 2
fi

fingerprint="$(printf '%s\n' "$fingerprints" | sed '/^$/d')"
[[ "$fingerprint" =~ ^[0-9a-f]{64}$ ]] || {
    echo "official publisher fingerprint: invalid value in $source_file" >&2
    exit 2
}

printf '%s\n' "$fingerprint"
