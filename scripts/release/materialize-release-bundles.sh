#!/usr/bin/env bash

# Lay out the released, officially signed bundles of one bundle set as an
# image build context: `<output>/.ai` plus `<output>/<bundle>` for each member
# of the set. The archive is verified in full (checksum, name, path safety,
# private-key scan, official publisher metadata) before anything is copied, and
# every publisher trust document that lands in the output must name the
# official publisher. Nothing here signs or re-signs: the bytes are the
# released bytes.

set -euo pipefail
export LC_ALL=C
root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$root/scripts/lib/ryeos-terminal.sh"
ryeos_term_init

usage() {
    ryeos_term_fail "usage: $0 --version X.Y.Z --archive PATH --checksum PATH --set BUNDLE_SET --output DIR"
    exit 2
}

version=""
archive=""
checksum=""
set_name=""
output=""
while (($#)); do
    case "$1" in
        --version) version="${2:-}"; shift 2 ;;
        --archive) archive="${2:-}"; shift 2 ;;
        --checksum) checksum="${2:-}"; shift 2 ;;
        --set) set_name="${2:-}"; shift 2 ;;
        --output) output="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$version" && -n "$archive" && -n "$checksum" && -n "$set_name" && -n "$output" ]] || usage

# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"
mapfile -t members < <(ryeos_bundle_set_names "$set_name") || true
[[ "${#members[@]}" -gt 0 && -n "${members[0]}" ]] || {
    ryeos_term_fail "unknown bundle set: $set_name"
    exit 2
}
if [[ -e "$output" ]]; then
    [[ -d "$output" && ! -L "$output" ]] || {
        ryeos_term_fail "output exists and is not a directory: $output"
        exit 2
    }
    [[ -z "$(find "$output" -mindepth 1 -print -quit)" ]] || {
        ryeos_term_fail "output directory must be empty: $output"
        exit 2
    }
fi

ryeos_term_begin PUBLISH "materializing released bundles"
# Full verification first; its failure aborts before any copy.
"$root/scripts/release/verify-bundle-artifact.sh" \
    --version "$version" \
    --archive "$archive" \
    --checksum "$checksum" >/dev/null

official_fp="$("$root/scripts/release/official-publisher-fingerprint.sh")"
archive_root="ryeos-bundles-${version}-x86_64"

tmp="$(mktemp -d)"
cleanup() {
    local status="$1"
    ryeos_term_handle_exit "$status"
    rm -rf "$tmp"
    return "$status"
}
trap 'cleanup "$?"' EXIT

tar --no-same-owner --no-same-permissions -xzf "$archive" -C "$tmp"
stage="$tmp/$archive_root"
[[ -d "$stage/.ai" ]] || {
    ryeos_term_fail "archive source-root .ai missing"
    exit 2
}
chmod -R u+rX "$stage"

assert_official_trust_doc() {
    local doc="$1" fingerprint owner
    [[ -f "$doc" ]] || return 1
    fingerprint="$(sed -n 's/^[[:space:]]*fingerprint[[:space:]]*=[[:space:]]*"\([0-9A-Fa-f]\{64\}\)"[[:space:]]*$/\1/p' "$doc")"
    owner="$(sed -n 's/^[[:space:]]*owner[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "$doc")"
    [[ "$(printf '%s\n' "$fingerprint" | sed '/^$/d' | wc -l)" -eq 1 \
        && "${fingerprint,,}" == "$official_fp" && "$owner" == ryeos-official ]]
}

assert_official_trust_doc "$stage/.ai/PUBLISHER_TRUST.toml" || {
    ryeos_term_fail "source-root publisher metadata is not the official publisher"
    exit 2
}
for bundle in "${members[@]}"; do
    [[ -d "$stage/$bundle/.ai" && ! -L "$stage/$bundle" ]] || {
        ryeos_term_fail "released archive is missing $bundle/.ai"
        exit 2
    }
    assert_official_trust_doc "$stage/$bundle/PUBLISHER_TRUST.toml" || {
        ryeos_term_fail "$bundle publisher metadata is not the official publisher"
        exit 2
    }
done

mkdir -p "$output"
cp -a "$stage/.ai" "$output/.ai"
for bundle in "${members[@]}"; do
    cp -a "$stage/$bundle" "$output/$bundle"
done

# Whatever else the copy carried, no trust document in the output may name
# another signer.
while IFS= read -r -d '' trust_doc; do
    assert_official_trust_doc "$trust_doc" || {
        ryeos_term_fail "non-official publisher metadata in output: $trust_doc"
        exit 2
    }
done < <(find "$output" -name PUBLISHER_TRUST.toml -print0)

ryeos_term_end success "MATERIALIZE COMPLETE" "$output"
