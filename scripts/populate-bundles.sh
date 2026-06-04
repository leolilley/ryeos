#!/usr/bin/env bash
# Populate bundles/*/.ai/bin/<triple>/ with freshly built
# binaries, then publish all bundles (sign items + rebuild CAS manifests).
#
# Use this whenever bundle bin/ contents are missing or stale:
#   - after a fresh checkout (binaries are .gitignored)
#   - before running tests that read the bundle tree (test_support.rs)
#   - before `docker build` if you want to skip the in-image cargo step
#
# Idempotent. Safe to re-run.
#
# Usage:
#   ./scripts/populate-bundles.sh --key <pem-path> --owner <label> [--bundle-set full|hosted-node]
#
# Env:
#   CARGO              cargo binary (default: cargo from PATH)
#   TRIPLE             host triple (default: x86_64-unknown-linux-gnu)

set -euo pipefail

# ── CLI parsing ──────────────────────────────────────────────────────

KEY=""
OWNER=""
BUNDLE_SET="full"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --key)   KEY="$2";   shift 2 ;;
    --owner) OWNER="$2"; shift 2 ;;
    --bundle-set) BUNDLE_SET="$2"; shift 2 ;;
    *) echo "populate-bundles.sh: unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$KEY"   ]]; then echo "populate-bundles.sh: --key <pem-path> is required"   >&2; exit 2; fi
if [[ -z "$OWNER" ]]; then echo "populate-bundles.sh: --owner <label> is required"    >&2; exit 2; fi
if [[ ! -s "$KEY" ]]; then echo "populate-bundles.sh: key file is empty or missing: $KEY" >&2; exit 2; fi
case "$BUNDLE_SET" in
  full|hosted-node) ;;
  *) echo "populate-bundles.sh: --bundle-set must be 'full' or 'hosted-node', got: $BUNDLE_SET" >&2; exit 2 ;;
esac

# ── Setup ────────────────────────────────────────────────────────────

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
TRIPLE="${TRIPLE:-x86_64-unknown-linux-gnu}"

# Resolve target directory: read from .cargo/config.toml or fallback
TARGET=""
if [ -f "$ROOT/.cargo/config.toml" ]; then
  TARGET="$(grep -o 'target-dir *= *"[^"]*' "$ROOT/.cargo/config.toml" 2>/dev/null | sed 's/.*"//' || true)"
fi
if [ -z "$TARGET" ]; then
  TARGET="$ROOT/target"
fi
echo "[populate-bundles] target dir: $TARGET"

CORE="$ROOT/bundles/core"
STD="$ROOT/bundles/standard"
WEB="$ROOT/bundles/web"
STUDIO="$ROOT/bundles/studio"
HOSTED_NODE="$ROOT/bundles/hosted-node"

case "$BUNDLE_SET" in
  full)
    BUNDLE_DIRS=("$CORE" "$STD" "$WEB" "$STUDIO" "$HOSTED_NODE")
    ;;
  hosted-node)
    BUNDLE_DIRS=("$CORE" "$HOSTED_NODE")
    ;;
esac

# ── Clean derived state from all bundles ────────────────────────────
# Wipe everything that will be regenerated so stale artifacts (old
# binaries, old manifests, old trust docs) don't leak through.

for BUNDLE_DIR in "${BUNDLE_DIRS[@]}"; do
  rm -rf "$BUNDLE_DIR/.ai/bin"
  rm -rf "$BUNDLE_DIR/.ai/objects"
  rm -rf "$BUNDLE_DIR/.ai/refs"
  rm -f  "$BUNDLE_DIR/PUBLISHER_TRUST.toml"
done

CORE_BIN="$CORE/.ai/bin/$TRIPLE"
STD_BIN="$STD/.ai/bin/$TRIPLE"
WEB_BIN="$WEB/.ai/bin/$TRIPLE"
STUDIO_BIN="$STUDIO/.ai/bin/$TRIPLE"
HOSTED_NODE_BIN="$HOSTED_NODE/.ai/bin/$TRIPLE"

case "$BUNDLE_SET" in
  full)
    mkdir -p "$CORE_BIN" "$STD_BIN" "$WEB_BIN" "$STUDIO_BIN" "$HOSTED_NODE_BIN"
    ;;
  hosted-node)
    mkdir -p "$CORE_BIN" "$HOSTED_NODE_BIN"
    ;;
esac

# ── Build ────────────────────────────────────────────────────────────

case "$BUNDLE_SET" in
  full)
    echo "[populate-bundles] building all release binaries (workspace)…"
    "$CARGO" build --release \
      -p ryeosd \
      -p ryeos-directive-runtime \
      -p ryeos-graph-runtime \
      -p ryeos-knowledge-runtime \
      -p ryeos-handler-bins \
      -p ryeos-cli \
      -p ryeos-tools \
      -p ryeos-web-tools \
      -p ryeos-ui-terminal \
      -p ryeos-ui-web
    ;;
  hosted-node)
    echo "[populate-bundles] building hosted-node release binaries…"
    "$CARGO" build --release \
      -p ryeosd \
      -p ryeos-handler-bins \
      -p ryeos-cli \
      -p ryeos-tools
    ;;
esac

# ── Stage binaries (only what each bundle owns) ──────────────────────

echo "[populate-bundles] installing core bundle binaries → $CORE_BIN"
install -m 0755 \
  "$TARGET/release/rye-parser-yaml-document" \
  "$TARGET/release/rye-parser-yaml-header-document" \
  "$TARGET/release/rye-parser-regex-kv" \
  "$TARGET/release/rye-composer-identity" \
  "$TARGET/release/ryeos-core-tools" \
  "$CORE_BIN/"

if [[ "$BUNDLE_SET" == "full" ]]; then
  echo "[populate-bundles] installing standard bundle binaries → $STD_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-directive-runtime" \
    "$TARGET/release/ryeos-graph-runtime" \
    "$TARGET/release/ryeos-knowledge-runtime" \
    "$TARGET/release/rye-composer-extends-chain" \
    "$TARGET/release/rye-composer-graph-permissions" \
    "$STD_BIN/"

  echo "[populate-bundles] installing studio bundle binaries → $STUDIO_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-tui" \
    "$TARGET/release/web" \
    "$STUDIO_BIN/"

  echo "[populate-bundles] installing web bundle binaries → $WEB_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-web-tools" \
    "$WEB_BIN/"
fi

# ── Publish ──────────────────────────────────────────────────────────

# Bundle publishing is an offline authoring operation. Use the maintainer
# binary directly rather than `ryeos publish`, because `publish` is no longer
# a lifecycle-local CLI verb on `next` and would otherwise route through a
# daemon/initialized-node dispatch path during Docker builds.
SIGN_USER_SPACE="$(mktemp -d)"
trap 'rm -rf "$SIGN_USER_SPACE"' EXIT
mkdir -p "$SIGN_USER_SPACE/.ai/config/keys/signing"
cp "$KEY" "$SIGN_USER_SPACE/.ai/config/keys/signing/private_key.pem"
chmod 0600 "$SIGN_USER_SPACE/.ai/config/keys/signing/private_key.pem"

echo "[populate-bundles] publishing core bundle…"
USER_SPACE="$SIGN_USER_SPACE" "$TARGET/release/ryeos-core-tools" build "$CORE" \
  --registry-root "$CORE" \
  --owner "$OWNER" >/dev/null

if [[ "$BUNDLE_SET" == "full" ]]; then
  echo "[populate-bundles] publishing standard bundle…"
  # Standard contains its own kind schemas (directive, graph, knowledge) now.
  # Core kinds are needed for verifying handlers/tools, so we pass core as registry-root.
  USER_SPACE="$SIGN_USER_SPACE" "$TARGET/release/ryeos-core-tools" build "$STD" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null

  echo "[populate-bundles] republishing core bundle with standard extension kinds…"
  # Core owns foundational runtime items but also ships documentation items whose
  # `knowledge` kind is provided by standard. Once standard has been signed, run
  # core through the authoring path again with both roots so those extension-kind
  # items are signed by the publisher key instead of being silently skipped.
  USER_SPACE="$SIGN_USER_SPACE" "$TARGET/release/ryeos-core-tools" build "$CORE" \
    --registry-root "$CORE" \
    --registry-root "$STD" \
    --owner "$OWNER" >/dev/null

  echo "[populate-bundles] publishing web bundle…"
  USER_SPACE="$SIGN_USER_SPACE" "$TARGET/release/ryeos-core-tools" build "$WEB" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null

  echo "[populate-bundles] publishing studio bundle…"
  USER_SPACE="$SIGN_USER_SPACE" "$TARGET/release/ryeos-core-tools" build "$STUDIO" \
    --registry-root "$CORE" \
    --registry-root "$STD" \
    --owner "$OWNER" >/dev/null
fi

echo "[populate-bundles] publishing hosted-node bundle…"
USER_SPACE="$SIGN_USER_SPACE" "$TARGET/release/ryeos-core-tools" build "$HOSTED_NODE" \
  --registry-root "$CORE" \
  --owner "$OWNER" >/dev/null

echo "[populate-bundles] done"
