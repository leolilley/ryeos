#!/usr/bin/env bash
# Populate ryeos-bundles/{core,standard}/.ai/bin/<triple>/ with freshly built
# binaries, then rebuild the bundle CAS manifests.
#
# Use this whenever bundle bin/ contents are missing or stale:
#   - after a fresh checkout (binaries are .gitignored)
#   - before running tests that read the bundle tree (test_support.rs)
#   - before `docker build` if you want to skip the in-image cargo step
#
# Idempotent. Safe to re-run.
#
# Env:
#   CARGO              cargo binary (default: cargo from PATH)
#   TRIPLE             host triple (default: x86_64-unknown-linux-gnu)
#   RYE_SIGNING_KEY    PEM path for bundle signing (default: ~/.ai/config/keys/signing/private_key.pem)
#                      If neither this nor the default file exists, falls back
#                      to deterministic --seed 1 (dev-only, not for production).
#   RYE_SIGNING_SEED   Seed for the deterministic fallback (default: 1)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
TRIPLE="${TRIPLE:-x86_64-unknown-linux-gnu}"
KEY_PATH="${RYE_SIGNING_KEY:-$HOME/.ai/config/keys/signing/private_key.pem}"
SEED="${RYE_SIGNING_SEED:-1}"

CORE="$ROOT/ryeos-bundles/core"
STD="$ROOT/ryeos-bundles/standard"
CORE_BIN="$CORE/.ai/bin/$TRIPLE"
STD_BIN="$STD/.ai/bin/$TRIPLE"

mkdir -p "$CORE_BIN" "$STD_BIN"

echo "[populate-bundles] building all release binaries (workspace)…"
"$CARGO" build --release \
  -p ryeosd \
  -p ryeos-directive-runtime \
  -p ryeos-graph-runtime \
  -p ryeos-knowledge-runtime \
  -p ryeos-handler-bins \
  -p ryeos-cli
"$CARGO" build --release -p ryeos-tools \
  --bin rye-bundle-tool --bin rye-inspect --bin rye-sign

echo "[populate-bundles] installing standard bundle binaries → $STD_BIN"
install -m 0755 \
  "$ROOT/target/release/ryeos-directive-runtime" \
  "$ROOT/target/release/ryeos-graph-runtime" \
  "$ROOT/target/release/ryeos-knowledge-runtime" \
  "$ROOT/target/release/rye" \
  "$STD_BIN/"

echo "[populate-bundles] installing core bundle binaries → $CORE_BIN"
install -m 0755 \
  "$ROOT/target/release/rye-parser-yaml-document" \
  "$ROOT/target/release/rye-parser-yaml-header-document" \
  "$ROOT/target/release/rye-parser-regex-kv" \
  "$ROOT/target/release/rye-composer-extends-chain" \
  "$ROOT/target/release/rye-composer-graph-permissions" \
  "$ROOT/target/release/rye-composer-identity" \
  "$ROOT/target/release/rye-inspect" \
  "$ROOT/target/release/rye-sign" \
  "$ROOT/target/release/rye-tool-streaming-demo" \
  "$CORE_BIN/"

# Pick signing key — prefer real PEM, fall back to deterministic seed.
if [[ -f "$KEY_PATH" ]]; then
  SIGN_ARGS=(--key "$KEY_PATH")
  echo "[populate-bundles] signing with key: $KEY_PATH"
else
  SIGN_ARGS=(--seed "$SEED")
  echo "[populate-bundles] no key at $KEY_PATH — using deterministic --seed $SEED (dev-only)"
fi

echo "[populate-bundles] rebuilding standard bundle manifest…"
"$ROOT/target/release/rye-bundle-tool" rebuild-manifest \
  --source "$STD" "${SIGN_ARGS[@]}" >/dev/null

echo "[populate-bundles] rebuilding core bundle manifest…"
"$ROOT/target/release/rye-bundle-tool" rebuild-manifest \
  --source "$CORE" "${SIGN_ARGS[@]}" >/dev/null

echo "[populate-bundles] done"
