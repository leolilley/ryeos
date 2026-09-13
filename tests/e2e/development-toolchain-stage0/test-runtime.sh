#!/usr/bin/env bash
# Cheap contract tests: no Docker, downloads, Rust compilation or node access.
set -euo pipefail
export LC_ALL=C
root="$(cd "$(dirname "$0")/../../.." && pwd)"
contract="$root/.ai/config/development/ryeos/stage0-platform-x86_64-linux.yaml"
helper="$root/.ai/tools/ryeos/development/stage0-platform-production/lib/runtime.sh"
verifier="$root/.ai/tools/ryeos/development/stage0-platform-production/lib/verify-bootstrap-artifact.sh"
producer="$root/scripts/release/produce-development-toolchain-stage0.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Exercise the moved verifier with the valid contract far enough to prove it
# resolves the canonical sibling runtime helper. The deliberately wrong archive
# name is rejected before any archive parsing or expensive tree verification.
: > "$tmp/not-stage0.tar.gz"
if bash "$verifier" --inputs "$contract" --producer "$producer" \
    --archive "$tmp/not-stage0.tar.gz" > "$tmp/verifier-error" 2>&1; then
    echo 'Stage-0 verifier accepted a wrongly named empty archive' >&2
    exit 1
fi
grep -Fq 'unexpected Stage-0 archive name' "$tmp/verifier-error" || {
    cat "$tmp/verifier-error" >&2
    echo 'Stage-0 verifier did not reach post-helper archive validation' >&2
    exit 1
}

# Run validation as a top-level command in a fresh shell. Putting the function
# in an `if` would disable errexit inside it and give false refusal evidence.
validate() {
    bash -euc '
        inputs="$1"
        rust_host=x86_64-unknown-linux-gnu
        contract_value() {
            awk -v key="$1" '\''$0 ~ "^" key ": " {
                sub(/^[^:]+: "/, ""); sub(/"$/, ""); print; count++
            } END {if (count != 1) exit 1}'\'' "$inputs"
        }
        source "$2"
        runtime_contract_validate
        if [[ -n "$3" ]]; then
            runtime_verify "$3" "$4"
        fi
    ' bash "$1" "$helper" "${2:-}" "$tmp"
}
if [[ "$#" -eq 2 && "$1" == --verify-tree ]]; then
    validate "$contract" "$2"
    exit
fi
[[ "$#" -eq 0 ]] || { echo 'usage: test-runtime.sh [--verify-tree DIR]' >&2; exit 2; }
validate "$contract"

refuse() {
    if validate "$tmp/input.yaml" > "$tmp/error" 2>&1; then
        echo "runtime contract unexpectedly accepted: $1" >&2
        exit 1
    fi
}

sed 's@runtime_mount: .*@runtime_mount: "/usr"@' "$contract" > "$tmp/input.yaml"
refuse 'ambient runtime mount'
sed 's@lib/libc.so.6 lib/libc.so@lib/libc.so.6 ../escape@' "$contract" > "$tmp/input.yaml"
refuse 'alias traversal'
sed 's@lib/libdl.so.2 lib/libdl.so@lib/libdl.so.2 lib/libc.so@' "$contract" > "$tmp/input.yaml"
refuse 'duplicate destination'
sed 's@patchelf_program_sha256: .*@patchelf_program_sha256: "latest"@' "$contract" > "$tmp/input.yaml"
refuse 'unpinned authoring program'
sed 's@/usr/lib/x86_64-linux-gnu/libc.so.6@/etc/libc.so.6@' "$contract" > "$tmp/input.yaml"
refuse 'undeclared image source namespace'
sed 's@lib/libc.so.6 755 1995216@lib/libc.so.6 777 1995216@' "$contract" > "$tmp/input.yaml"
refuse 'writable runtime member'
sed '/^image_member_/d' "$contract" > "$tmp/input.yaml"
refuse 'missing image inputs'
sed '/^runtime_alias_/d' "$contract" > "$tmp/input.yaml"
refuse 'missing native aliases'
sed '$a runtime_alias_c: "lib/libc.so.6 lib/extra.so"' "$contract" > "$tmp/input.yaml"
refuse 'duplicate contract key'
echo 'Stage-0 runtime input contract tests passed (valid + 9 refusals)'
