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
#   ./scripts/populate-bundles.sh --key <pem-path> --owner <label> [--bundle-set full|standard|hosted-node|hosted-workflow]
#
# Bundle sets:
#   full            core + standard + web + browser + studio + hosted-node (default)
#   standard        core + standard — scheduler/graph/directive standard node
#   hosted-node     core + hosted-node — lean remote-admission control plane
#   hosted-workflow core + standard + hosted-node — hosted node that also
#                   runs scheduler/graph/directive workloads
#
# Env:
#   CARGO              cargo binary (default: cargo from PATH)
#   CARGO_TARGET_DIR   cargo target dir (default: .cargo/config target-dir or ./target)
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
if ! command -v openssl >/dev/null 2>&1; then echo "populate-bundles.sh: openssl is required" >&2; exit 2; fi
if ! command -v sha256sum >/dev/null 2>&1; then echo "populate-bundles.sh: sha256sum is required" >&2; exit 2; fi
if ! command -v base64 >/dev/null 2>&1; then echo "populate-bundles.sh: base64 is required" >&2; exit 2; fi
case "$BUNDLE_SET" in
  full|standard|hosted-node|hosted-workflow) ;;
  *) echo "populate-bundles.sh: --bundle-set must be 'full', 'standard', 'hosted-node', or 'hosted-workflow', got: $BUNDLE_SET" >&2; exit 2 ;;
esac

base64_one_line() {
  base64 -w0 2>/dev/null || base64 | tr -d '\n'
}

publisher_pubkey_raw_b64() {
  openssl pkey -in "$KEY" -pubout -outform DER 2>/dev/null \
    | tail -c 32 \
    | base64_one_line
}

publisher_fingerprint() {
  openssl pkey -in "$KEY" -pubout -outform DER 2>/dev/null \
    | tail -c 32 \
    | sha256sum \
    | cut -d' ' -f1
}

sign_seed_yaml() {
  local file="$1"
  local body_tmp hash_tmp sig_tmp tmp timestamp hash sig

  [[ -f "$file" ]] || { echo "populate-bundles.sh: seed YAML missing: $file" >&2; exit 2; }
  body_tmp="$(mktemp)"
  hash_tmp="$(mktemp)"
  sig_tmp="$(mktemp)"
  tmp="$file.tmp.$$"

  sed '/^# ryeos:signed:/d' "$file" > "$body_tmp"
  hash="$(sha256sum "$body_tmp" | cut -d' ' -f1)"
  printf '%s' "$hash" > "$hash_tmp"
  openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$hash_tmp" -out "$sig_tmp" 2>/dev/null
  sig="$(base64_one_line < "$sig_tmp")"
  timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

  {
    printf '# ryeos:signed:%s:%s:%s:%s\n' "$timestamp" "$hash" "$sig" "$PUBLISHER_FP"
    cat "$body_tmp"
  } > "$tmp"
  mv "$tmp" "$file"
  rm -f "$body_tmp" "$hash_tmp" "$sig_tmp"
}

write_seed_trust_doc() {
  local target="$ROOT/bundles/.ai/PUBLISHER_TRUST.toml"
  cat > "$target" <<EOF
public_key = "ed25519:$PUBLISHER_PUBKEY_RAW_B64"
fingerprint = "$PUBLISHER_FP"
owner = "$OWNER"
EOF
}

assert_no_legacy_seed_paths() {
  local stale
  for stale in \
    "$SOURCE_ROOT_AI/node/command_registration" \
    "$SOURCE_ROOT_AI/node/bundle_registration_grants"
  do
    if [[ -e "$stale" ]]; then
      echo "populate-bundles.sh: stale legacy source-root seed path exists: $stale" >&2
      exit 2
    fi
  done
}

# ── Setup ────────────────────────────────────────────────────────────

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
TRIPLE="${TRIPLE:-x86_64-unknown-linux-gnu}"

# Resolve target directory: prefer Cargo's env override, then .cargo/config.toml,
# then the workspace default. Keep this in sync with the cargo invocation so the
# binary install step reads from the directory Cargo actually wrote.
TARGET="${CARGO_TARGET_DIR:-}"
if [ -z "$TARGET" ]; then
  if [ -f "$ROOT/.cargo/config.toml" ]; then
    TARGET="$(grep -o 'target-dir *= *"[^"]*' "$ROOT/.cargo/config.toml" 2>/dev/null | sed 's/.*"//' || true)"
  fi
fi
if [ -z "$TARGET" ]; then
  TARGET="$ROOT/target"
elif [[ "$TARGET" != /* ]]; then
  TARGET="$ROOT/$TARGET"
fi
echo "[populate-bundles] target dir: $TARGET"

CORE="$ROOT/bundles/core"
STD="$ROOT/bundles/standard"
WEB="$ROOT/bundles/web"
BROWSER="$ROOT/bundles/browser"
STUDIO="$ROOT/bundles/studio"
HOSTED_NODE="$ROOT/bundles/hosted-node"
SOURCE_ROOT_AI="$ROOT/bundles/.ai"
INIT_SEED="$SOURCE_ROOT_AI/node/init"
PUBLISHER_PUBKEY_RAW_B64="$(publisher_pubkey_raw_b64)"
PUBLISHER_FP="$(publisher_fingerprint)"

case "$BUNDLE_SET" in
  full)
    BUNDLE_DIRS=("$CORE" "$STD" "$WEB" "$BROWSER" "$STUDIO" "$HOSTED_NODE")
    ;;
  standard)
    BUNDLE_DIRS=("$CORE" "$STD")
    ;;
  hosted-node)
    BUNDLE_DIRS=("$CORE" "$HOSTED_NODE")
    ;;
  hosted-workflow)
    BUNDLE_DIRS=("$CORE" "$STD" "$HOSTED_NODE")
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
BROWSER_BIN="$BROWSER/.ai/bin/$TRIPLE"
STUDIO_BIN="$STUDIO/.ai/bin/$TRIPLE"
HOSTED_NODE_BIN="$HOSTED_NODE/.ai/bin/$TRIPLE"

case "$BUNDLE_SET" in
  full)
    mkdir -p "$CORE_BIN" "$STD_BIN" "$WEB_BIN" "$BROWSER_BIN" "$STUDIO_BIN" "$HOSTED_NODE_BIN"
    ;;
  standard)
    mkdir -p "$CORE_BIN" "$STD_BIN"
    ;;
  hosted-node)
    mkdir -p "$CORE_BIN" "$HOSTED_NODE_BIN"
    ;;
  hosted-workflow)
    mkdir -p "$CORE_BIN" "$STD_BIN" "$HOSTED_NODE_BIN"
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
      -p ryeos-browser-tools \
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
  standard)
    echo "[populate-bundles] building standard release binaries…"
    "$CARGO" build --release \
      -p ryeosd \
      -p ryeos-directive-runtime \
      -p ryeos-graph-runtime \
      -p ryeos-knowledge-runtime \
      -p ryeos-handler-bins \
      -p ryeos-cli \
      -p ryeos-tools
    ;;
  hosted-workflow)
    echo "[populate-bundles] building hosted-workflow release binaries…"
    "$CARGO" build --release \
      -p ryeosd \
      -p ryeos-directive-runtime \
      -p ryeos-graph-runtime \
      -p ryeos-knowledge-runtime \
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

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "standard" || "$BUNDLE_SET" == "hosted-workflow" ]]; then
  echo "[populate-bundles] installing standard bundle binaries → $STD_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-directive-runtime" \
    "$TARGET/release/ryeos-graph-runtime" \
    "$TARGET/release/ryeos-knowledge-runtime" \
    "$TARGET/release/rye-composer-extends-chain" \
    "$TARGET/release/rye-composer-graph-permissions" \
    "$STD_BIN/"
fi

if [[ "$BUNDLE_SET" == "full" ]]; then
  echo "[populate-bundles] installing studio bundle binaries → $STUDIO_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-tui" \
    "$TARGET/release/web" \
    "$STUDIO_BIN/"

  echo "[populate-bundles] installing web bundle binaries → $WEB_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-web-tools" \
    "$WEB_BIN/"

  echo "[populate-bundles] installing browser bundle binaries → $BROWSER_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-browser-tools" \
    "$BROWSER_BIN/"
fi

# ── Publish ──────────────────────────────────────────────────────────

echo "[populate-bundles] signing source-root seed data…"
assert_no_legacy_seed_paths
sign_seed_yaml "$INIT_SEED/command-registration/default.yaml"
sign_seed_yaml "$INIT_SEED/bundle-registration-grants/default.yaml"
write_seed_trust_doc

# Bundle publishing is an offline authoring operation. Use the maintainer
# binary directly rather than `ryeos publish`, because `publish` is no longer
# a lifecycle-local CLI verb on `next` and would otherwise route through a
# daemon/initialized-node dispatch path during Docker builds.
SIGN_APP_ROOT="$(mktemp -d)"
trap 'rm -rf "$SIGN_APP_ROOT"' EXIT
mkdir -p "$SIGN_APP_ROOT/.ai/config/keys/signing"
cp "$KEY" "$SIGN_APP_ROOT/.ai/config/keys/signing/private_key.pem"
chmod 0600 "$SIGN_APP_ROOT/.ai/config/keys/signing/private_key.pem"

echo "[populate-bundles] publishing core bundle…"
RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$CORE" \
  --registry-root "$CORE" \
  --owner "$OWNER" >/dev/null

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "standard" || "$BUNDLE_SET" == "hosted-workflow" ]]; then
  echo "[populate-bundles] publishing standard bundle…"
  # Standard contains its own kind schemas (directive, graph, knowledge) now.
  # Core kinds are needed for verifying handlers/tools, so we pass core as registry-root.
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$STD" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null

  echo "[populate-bundles] republishing core bundle with standard extension kinds…"
  # Core owns foundational runtime items but also ships documentation items whose
  # `knowledge` kind is provided by standard. Once standard has been signed, run
  # core through the authoring path again with both roots so those extension-kind
  # items are signed by the publisher key instead of being silently skipped.
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$CORE" \
    --registry-root "$CORE" \
    --registry-root "$STD" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full" ]]; then
  echo "[populate-bundles] publishing web bundle…"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$WEB" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null

  echo "[populate-bundles] publishing browser bundle…"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$BROWSER" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null

  echo "[populate-bundles] publishing studio bundle…"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$STUDIO" \
    --registry-root "$CORE" \
    --registry-root "$STD" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "hosted-node" || "$BUNDLE_SET" == "hosted-workflow" ]]; then
  echo "[populate-bundles] publishing hosted-node bundle…"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$HOSTED_NODE" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null
fi

echo "[populate-bundles] done"
