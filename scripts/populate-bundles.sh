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
#   ./scripts/populate-bundles.sh --key <pem-path> --owner <label> [--bundle-set full|central-host|standard|hosted-node|hosted-workflow]
#
# Bundle sets:
#   full            core + standard + web + browser + ryeos-ui + hosted-node (default)
#   central-host    core + standard + web — standard node plus the rye/web/search
#                   tool; the app-hosting image (e.g. tv-tracker) that also serves
#                   its own central-auth realm
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
JOBS=""            # cargo -j N; empty = cargo default (all cores)
CRATES_OVERRIDE="" # space-separated crate list; empty = bundle-set default
POPULATE_ALL=0     # explicit opt-in to rebuild the whole bundle set

while [[ $# -gt 0 ]]; do
  case "$1" in
    --key)   KEY="$2";   shift 2 ;;
    --owner) OWNER="$2"; shift 2 ;;
    --bundle-set) BUNDLE_SET="$2"; shift 2 ;;
    --jobs)  JOBS="$2";  shift 2 ;;
    # Rebuild only these crates instead of the whole bundle set. Staging still
    # copies every bundle binary from target/release, so the others must already
    # be built (e.g. from a prior populate). Use when iterating on one binary —
    # `--crates ryeos-core-tools` rebuilds core-tools without the full workspace.
    --crates) CRATES_OVERRIDE="$2"; shift 2 ;;
    --all) POPULATE_ALL=1; shift ;;
    *) echo "populate-bundles.sh: unknown arg: $1" >&2; exit 2 ;;
  esac
done

# Refuse to build the whole workspace implicitly — that full release build is
# what exhausts memory. The caller must be explicit: name the crates that
# changed, or opt into the whole set with --all.
if [[ -z "$CRATES_OVERRIDE" && "$POPULATE_ALL" -ne 1 ]]; then
  echo "populate-bundles.sh: refusing to rebuild the full bundle set implicitly." >&2
  echo "  Pass --crates \"<crate ...>\" to rebuild only what changed (e.g. --crates ryeos-core-tools)," >&2
  echo "  or --all to rebuild the whole '$BUNDLE_SET' set." >&2
  exit 2
fi

if [[ -z "$KEY"   ]]; then echo "populate-bundles.sh: --key <pem-path> is required"   >&2; exit 2; fi
if [[ -z "$OWNER" ]]; then echo "populate-bundles.sh: --owner <label> is required"    >&2; exit 2; fi
if [[ ! -s "$KEY" ]]; then echo "populate-bundles.sh: key file is empty or missing: $KEY" >&2; exit 2; fi
if ! command -v openssl >/dev/null 2>&1; then echo "populate-bundles.sh: openssl is required" >&2; exit 2; fi
if ! command -v sha256sum >/dev/null 2>&1; then echo "populate-bundles.sh: sha256sum is required" >&2; exit 2; fi
if ! command -v base64 >/dev/null 2>&1; then echo "populate-bundles.sh: base64 is required" >&2; exit 2; fi
case "$BUNDLE_SET" in
  full|central-host|standard|hosted-node|hosted-workflow) ;;
  *) echo "populate-bundles.sh: --bundle-set must be 'full', 'central-host', 'standard', 'hosted-node', or 'hosted-workflow', got: $BUNDLE_SET" >&2; exit 2 ;;
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

  # Idempotent: the signature covers the body hash only (the timestamp is
  # envelope metadata), and ed25519 is deterministic, so an unchanged body
  # yields the same hash:sig:fp tail. If the existing signature already matches,
  # leave the file untouched — re-stamping a fresh timestamp would churn the
  # committed seed files on every populate run for no content change.
  if head -1 "$file" | grep -qF ":${hash}:${sig}:${PUBLISHER_FP}"; then
    rm -f "$body_tmp" "$hash_tmp" "$sig_tmp"
    return 0
  fi

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

# Shared bundle-set definition (one source of truth with install-local-direct.sh).
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$ROOT/scripts/pkg/bundle-sets.sh"

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
RYEOS_UI="$ROOT/bundles/ryeos-ui"
HOSTED_NODE="$ROOT/bundles/hosted-node"
TVTA="$ROOT/bundles/tv-tracker-authoring"
SOURCE_ROOT_AI="$ROOT/bundles/.ai"
INIT_SEED="$SOURCE_ROOT_AI/node/init"
PUBLISHER_PUBKEY_RAW_B64="$(publisher_pubkey_raw_b64)"
PUBLISHER_FP="$(publisher_fingerprint)"

# Bin-managed bundles for this set come from the shared definition (central-auth
# is excluded — it owns no compiled binaries and is published unconditionally
# below, so it must never be cleaned/staged here).
BUNDLE_DIRS=()
while IFS= read -r _bundle_name; do
  BUNDLE_DIRS+=("$ROOT/bundles/$_bundle_name")
done < <(ryeos_bundle_set_bin_managed_names "$BUNDLE_SET")

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
RYEOS_UI_BIN="$RYEOS_UI/.ai/bin/$TRIPLE"
HOSTED_NODE_BIN="$HOSTED_NODE/.ai/bin/$TRIPLE"

# Bin dirs for exactly the bin-managed bundles this set builds.
for BUNDLE_DIR in "${BUNDLE_DIRS[@]}"; do
  mkdir -p "$BUNDLE_DIR/.ai/bin/$TRIPLE"
done

# ── Build ────────────────────────────────────────────────────────────

# Crate list per bundle set (the default when --crates is not given).
case "$BUNDLE_SET" in
  full)
    pkgs=(ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-cli ryeos-core-tools ryeos-web-tools ryeos-browser-tools \
          ryeos-ui-terminal ryeos-ui-web)
    ;;
  central-host)
    pkgs=(ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-cli ryeos-core-tools ryeos-web-tools)
    ;;
  standard|hosted-workflow)
    pkgs=(ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-cli ryeos-core-tools)
    ;;
  hosted-node)
    pkgs=(ryeosd ryeos-handler-bins ryeos-cli ryeos-core-tools)
    ;;
esac

# --crates overrides the build list (staging still copies all bundle binaries
# from target/release, so unbuilt ones must already exist there).
if [[ -n "$CRATES_OVERRIDE" ]]; then
  read -ra pkgs <<< "$CRATES_OVERRIDE"
fi

build_args=()
for p in "${pkgs[@]}"; do build_args+=(-p "$p"); done
jobs_args=()
[[ -n "$JOBS" ]] && jobs_args=(-j "$JOBS")

echo "[populate-bundles] building release binaries${JOBS:+ (jobs=$JOBS)}: ${pkgs[*]}"
"$CARGO" build --release "${jobs_args[@]}" "${build_args[@]}"

# ── Guard: no stale sibling binaries under --crates ──────────────────
# With --crates only the named crates are rebuilt, but staging copies EVERY
# bundle binary from target/release. A sibling built before a foundational lib
# (ryeos-runtime / ryeos-state / ryeos-app) changed would stage linked against
# the old lib and silently drift. Fail loudly, naming the stale binaries.

# Release binaries this set stages, one per line (mirrors the staging steps).
staged_release_bins_for_set() {
  printf '%s\n' \
    rye-parser-yaml-document rye-parser-yaml-header-document rye-parser-regex-kv \
    rye-composer-identity ryeos-core-tools
  case "$BUNDLE_SET" in
    full|central-host|standard|hosted-workflow)
      printf '%s\n' ryeos-directive-runtime ryeos-graph-runtime \
        ryeos-knowledge-runtime rye-composer-extends-chain rye-composer-graph-permissions
      ;;
  esac
  case "$BUNDLE_SET" in
    full) printf '%s\n' ryeos-tui web ryeos-web-tools ryeos-browser-tools ;;
    central-host) printf '%s\n' ryeos-web-tools ;;
  esac
}

# Newest mtime (integer epoch) across the foundational library crate sources.
foundational_newest_mtime() {
  find \
    "$ROOT/crates/engine/ryeos-runtime/src" \
    "$ROOT/crates/state/ryeos-state/src" \
    "$ROOT/crates/daemon/ryeos-app/src" \
    -type f -name '*.rs' -printf '%T@\n' 2>/dev/null \
    | sort -rn | head -1 | cut -d. -f1
}

if [[ -n "$CRATES_OVERRIDE" ]]; then
  _newest_foundational="$(foundational_newest_mtime)"
  if [[ -n "$_newest_foundational" ]]; then
    _stale=()
    while IFS= read -r _bin; do
      [[ -n "$_bin" ]] || continue
      _bin_path="$TARGET/release/$_bin"
      [[ -f "$_bin_path" ]] || continue
      _bin_mtime="$(stat -c %Y "$_bin_path" 2>/dev/null || echo 0)"
      if (( _bin_mtime < _newest_foundational )); then
        _stale+=("$_bin")
      fi
    done < <(staged_release_bins_for_set)
    if (( ${#_stale[@]} > 0 )); then
      {
        echo "populate-bundles.sh: refusing to stage binaries older than the foundational libs."
        echo "  The foundational crates (ryeos-runtime / ryeos-state / ryeos-app) have source newer"
        echo "  than these staged binaries, so they would link against a stale lib:"
        printf '    - %s\n' "${_stale[@]}"
        echo "  Rebuild the whole '$BUNDLE_SET' set:"
        echo "    ./scripts/populate-bundles.sh --key \"$KEY\" --owner \"$OWNER\" --bundle-set \"$BUNDLE_SET\" --all"
      } >&2
      exit 2
    fi
  fi
fi

# ── Stage binaries (only what each bundle owns) ──────────────────────

echo "[populate-bundles] installing core bundle binaries → $CORE_BIN"
install -m 0755 \
  "$TARGET/release/rye-parser-yaml-document" \
  "$TARGET/release/rye-parser-yaml-header-document" \
  "$TARGET/release/rye-parser-regex-kv" \
  "$TARGET/release/rye-composer-identity" \
  "$TARGET/release/ryeos-core-tools" \
  "$CORE_BIN/"

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "central-host" || "$BUNDLE_SET" == "standard" || "$BUNDLE_SET" == "hosted-workflow" ]]; then
  echo "[populate-bundles] installing standard bundle binaries → $STD_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-directive-runtime" \
    "$TARGET/release/ryeos-graph-runtime" \
    "$TARGET/release/ryeos-knowledge-runtime" \
    "$TARGET/release/rye-composer-extends-chain" \
    "$TARGET/release/rye-composer-graph-permissions" \
    "$STD_BIN/"
fi

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "central-host" ]]; then
  echo "[populate-bundles] installing web bundle binaries → $WEB_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-web-tools" \
    "$WEB_BIN/"
fi

if [[ "$BUNDLE_SET" == "full" ]]; then
  echo "[populate-bundles] installing ryeos-ui bundle binaries → $RYEOS_UI_BIN"
  install -m 0755 \
    "$TARGET/release/ryeos-tui" \
    "$TARGET/release/web" \
    "$RYEOS_UI_BIN/"

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

# central-auth ships in the source tree and is discovered/parsed at init, so its
# manifest must stay current with the manifest schema. It depends only on core's
# tool + config kinds, so publish it right after core (now that core carries a
# published refs root) with core as its registry root.
echo "[populate-bundles] publishing central-auth bundle…"
RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$ROOT/bundles/central-auth" \
  --registry-root "$CORE" \
  --owner "$OWNER" >/dev/null

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "central-host" || "$BUNDLE_SET" == "standard" || "$BUNDLE_SET" == "hosted-workflow" ]]; then
  echo "[populate-bundles] publishing standard bundle…"
  # Standard contains its own kind schemas (directive, graph, knowledge) now.
  # Core kinds are needed for verifying handlers/tools, so we pass core as registry-root.
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$STD" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "central-host" ]]; then
  echo "[populate-bundles] publishing web bundle…"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$WEB" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "central-host" ]]; then
  # tv-tracker-authoring — source-only bundle (tool kind from core); ships the
  # operator context-doc author/read wrappers. No compiled binary of its own.
  echo "[populate-bundles] publishing tv-tracker-authoring bundle…"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$TVTA" \
    --registry-root "$CORE" \
    --registry-root "$STD" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full" ]]; then
  echo "[populate-bundles] publishing browser bundle…"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$BROWSER" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null

  echo "[populate-bundles] publishing ryeos-ui bundle…"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$TARGET/release/ryeos-core-tools" build "$RYEOS_UI" \
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
