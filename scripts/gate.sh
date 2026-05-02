#!/usr/bin/env bash
# Workspace gate: auto-fixes the rye-inspect symlink/manifest hash
# mismatch documented in docs/operations/dev-tree-caveats.md, then
# runs the full nextest workspace gate, then verifies that committed
# bundle binaries are in sync with source.
#
# Usage:
#     ./scripts/gate.sh                 # full workspace
#     ./scripts/gate.sh -p ryeosd       # forwarded to nextest
#     ./scripts/gate.sh --no-tests      # only sync manifest, skip tests
#     ./scripts/gate.sh --no-bundle     # skip bundle drift check
#
# This is the canonical gate; CI and humans should both invoke it.
# Calling cargo test / cargo nextest run directly is fine but skips
# the auto-sync and may surface the hash-mismatch failures.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-/home/leo/.local/share/cargo/bin/cargo}"
KEY="${RYE_SIGNING_KEY:-$HOME/.ai/config/keys/signing/private_key.pem}"
TRIPLE="${TRIPLE:-x86_64-unknown-linux-gnu}"

BUNDLE="$ROOT/ryeos-bundles/core"
BIN_DIR="$BUNDLE/.ai/bin/$TRIPLE"
MANIFEST="$BIN_DIR/MANIFEST.json"
INSPECT_LINK="$BIN_DIR/rye-inspect"

skip_tests=0
skip_bundle=0
nextest_args=()
for arg in "$@"; do
    case "$arg" in
        --no-tests) skip_tests=1 ;;
        --no-bundle) skip_bundle=1 ;;
        *) nextest_args+=("$arg") ;;
    esac
done

if [[ ! -L "$INSPECT_LINK" ]]; then
    echo "gate: $INSPECT_LINK is not a symlink — nothing to sync, running gate directly" >&2
else
    echo "gate: rebuilding rye-inspect (cheap if cached) before hash-check"
    "$CARGO" build --bin rye-inspect >/dev/null

    on_disk="$(sha256sum "$INSPECT_LINK" | awk '{print $1}')"
    in_manifest="$(jq -r '."rye-inspect".blob_hash // empty' "$MANIFEST")"

    if [[ -z "$in_manifest" ]]; then
        echo "gate: rye-inspect not present in manifest — running rebuild-manifest"
        rebuild=1
    elif [[ "$on_disk" != "$in_manifest" ]]; then
        echo "gate: hash drift — on-disk=$on_disk manifest=$in_manifest"
        rebuild=1
    else
        echo "gate: rye-inspect hash matches manifest ($on_disk)"
        rebuild=0
    fi

    if [[ "$rebuild" == "1" ]]; then
        echo "gate: running rye-bundle-tool rebuild-manifest --source $BUNDLE"
        "$CARGO" run -q --bin rye-bundle-tool -- \
            rebuild-manifest --source "$BUNDLE" --key "$KEY" >/dev/null
        echo "gate: manifest re-signed with platform-author key"
    fi
fi

if [[ "$skip_tests" == "1" ]]; then
    exit 0
fi

echo "gate: cargo nextest run --workspace --no-fail-fast ${nextest_args[*]:-}"
"$CARGO" nextest run --workspace --no-fail-fast "${nextest_args[@]:-}"

if [[ "$skip_bundle" == "0" ]] && [[ "$(uname -m)" == "x86_64" ]] && [[ "$(uname -s)" == "Linux" ]]; then
    echo "gate: verifying bundle binaries are up-to-date with source"
    echo "gate: running rye-bundle-tool rebuild-manifest --source $BUNDLE"
    "$CARGO" run -q --bin rye-bundle-tool -- \
        rebuild-manifest --source "$BUNDLE" --key "$KEY" >/dev/null

    if ! git -C "$ROOT" diff --exit-code ryeos-bundles/ >/dev/null; then
        echo "gate: FAIL — bundle binaries are out of date with source" >&2
        echo "gate: run: rye-bundle-tool rebuild-manifest --source $BUNDLE --key \$KEY" >&2
        echo "gate: then commit the updated ryeos-bundles/" >&2
        git -C "$ROOT" diff --stat ryeos-bundles/ >&2
        exit 1
    fi
    echo "gate: bundle binaries match committed state"
else
    if [[ "$skip_bundle" == "0" ]]; then
        echo "gate: skipping bundle drift check (not x86_64-linux)"
    fi
fi
