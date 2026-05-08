#!/usr/bin/env bash
# Populate ryeos-bundles/{core,standard}/.ai/bin/<triple>/ with freshly built
# binaries, then publish both bundles (sign items + rebuild CAS manifests).
#
# Use this whenever bundle bin/ contents are missing or stale:
#   - after a fresh checkout (binaries are .gitignored)
#   - before running tests that read the bundle tree (test_support.rs)
#   - before `docker build` if you want to skip the in-image cargo step
#
# Idempotent. Safe to re-run.
#
# Usage:
#   ./scripts/populate-bundles.sh --key <pem-path> --owner <label>
#
# Env:
#   CARGO              cargo binary (default: cargo from PATH)
#   TRIPLE             host triple (default: x86_64-unknown-linux-gnu)

set -euo pipefail

# ── CLI parsing ──────────────────────────────────────────────────────

KEY=""
OWNER=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --key)   KEY="$2";   shift 2 ;;
    --owner) OWNER="$2"; shift 2 ;;
    *) echo "populate-bundles.sh: unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$KEY"   ]]; then echo "populate-bundles.sh: --key <pem-path> is required"   >&2; exit 2; fi
if [[ -z "$OWNER" ]]; then echo "populate-bundles.sh: --owner <label> is required"    >&2; exit 2; fi
if [[ ! -s "$KEY" ]]; then echo "populate-bundles.sh: key file is empty or missing: $KEY" >&2; exit 2; fi

# ── Setup ────────────────────────────────────────────────────────────

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
TRIPLE="${TRIPLE:-x86_64-unknown-linux-gnu}"

CORE="$ROOT/ryeos-bundles/core"
STD="$ROOT/ryeos-bundles/standard"

# ── Clean derived state from both bundles ────────────────────────────
# Wipe everything that will be regenerated so stale artifacts (old
# binaries, old manifests, old trust docs) don't leak through.

for BUNDLE in core standard; do
  BUNDLE_DIR="$ROOT/ryeos-bundles/$BUNDLE"
  rm -rf "$BUNDLE_DIR/.ai/bin"
  rm -rf "$BUNDLE_DIR/.ai/objects"
  rm -rf "$BUNDLE_DIR/.ai/refs"
  rm -f  "$BUNDLE_DIR/PUBLISHER_TRUST.toml"
done

CORE_BIN="$CORE/.ai/bin/$TRIPLE"
STD_BIN="$STD/.ai/bin/$TRIPLE"

mkdir -p "$CORE_BIN" "$STD_BIN"

# ── Build ────────────────────────────────────────────────────────────

echo "[populate-bundles] building all release binaries (workspace)…"
"$CARGO" build --release \
  -p ryeosd \
  -p ryeos-directive-runtime \
  -p ryeos-graph-runtime \
  -p ryeos-knowledge-runtime \
  -p ryeos-handler-bins \
  -p ryeos-cli \
  -p ryeos-tools

# ── Stage binaries (only what each bundle owns) ──────────────────────

echo "[populate-bundles] installing standard bundle binaries → $STD_BIN"
install -m 0755 \
  "$ROOT/target/release/ryeos-directive-runtime" \
  "$ROOT/target/release/ryeos-graph-runtime" \
  "$ROOT/target/release/ryeos-knowledge-runtime" \
  "$STD_BIN/"

echo "[populate-bundles] installing core bundle binaries → $CORE_BIN"
install -m 0755 \
  "$ROOT/target/release/rye-parser-yaml-document" \
  "$ROOT/target/release/rye-parser-yaml-header-document" \
  "$ROOT/target/release/rye-parser-regex-kv" \
  "$ROOT/target/release/rye-composer-extends-chain" \
  "$ROOT/target/release/rye-composer-graph-permissions" \
  "$ROOT/target/release/rye-composer-identity" \
  "$ROOT/target/release/ryeos-core-tools" \
  "$CORE_BIN/"

# ── Publish ──────────────────────────────────────────────────────────

echo "[populate-bundles] publishing core bundle…"
"$ROOT/target/release/ryeos" publish "$CORE" \
  --registry-root "$CORE" \
  --key "$KEY" \
  --owner "$OWNER" >/dev/null

echo "[populate-bundles] publishing standard bundle…"
"$ROOT/target/release/ryeos" publish "$STD" \
  --registry-root "$CORE" \
  --key "$KEY" \
  --owner "$OWNER" >/dev/null

echo "[populate-bundles] done"
