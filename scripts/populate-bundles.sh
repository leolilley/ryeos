#!/usr/bin/env bash
# Populate bundles/*/.ai/bin/<triple>/ from selected freshly built packages and
# retained exact artifact generations, then publish the selected bundle set.
#
# Use this whenever bundle bin/ contents are missing or stale:
#   - after a fresh checkout (binaries are .gitignored)
#   - before running tests that read the bundle tree (test_support.rs)
#   - before `docker build` if you want to skip the in-image cargo step
#
# Idempotent. Safe to re-run.
#
# Usage:
#   ./scripts/populate-bundles.sh --key <pem-path> --owner <label> [--bundle-set full|full-sandbox|central-host|standard|hosted-node|hosted-workflow|release-artifacts] (--crates "<package ...>" | --all) [--build-profile release|latency-profiling]
#
# Bundle sets:
#   full            core + central-auth + standard + web + browser + ryeos-ui +
#                   hosted-node + codex + local-inference (default)
#   full-sandbox    full + the separately authored Linux isolation backend;
#                   build its payload explicitly first
#   central-host    core + central-auth + standard + web + tv-tracker-authoring —
#                   standard node plus the rye/web/search tool and app authoring
#   standard        core + central-auth + standard — scheduler/graph/directive node
#   hosted-node     core + central-auth + hosted-node — lean remote-admission plane
#   hosted-workflow core + central-auth + standard + hosted-node + codex — hosted
#                   node that also runs scheduler/graph/directive and Codex workloads
#   release-artifacts internal non-installable union used to compile and publish
#                   the native archive and every release image in one build
#
# Env:
#   CARGO              cargo binary (default: cargo from PATH)
#   CARGO_TARGET_DIR   cargo target dir (default: .cargo/config target-dir or ./target)
#   TRIPLE             host triple (default: x86_64-unknown-linux-gnu)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$ROOT/scripts/lib/ryeos-terminal.sh"
ryeos_term_init

# ── CLI parsing ──────────────────────────────────────────────────────

KEY=""
OWNER=""
BUNDLE_SET="full"
JOBS=""            # cargo -j N; empty = cargo default (all cores)
CRATES_OVERRIDE="" # space-separated Cargo package list; empty = bundle-set default
POPULATE_ALL=0     # explicit opt-in to rebuild the whole bundle set
# The build profile is an explicit artifact property, not ambient process
# configuration. Containers pass the same closed value through this CLI.
BUILD_PROFILE="release"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --key)   KEY="$2";   shift 2 ;;
    --owner) OWNER="$2"; shift 2 ;;
    --bundle-set) BUNDLE_SET="$2"; shift 2 ;;
    --jobs)  JOBS="$2";  shift 2 ;;
    # Rebuild only these Cargo packages. Staging retains the exact existing
    # artifact generation for every unselected payload, so those outputs must
    # already exist (e.g. from a prior full population). Static worker packages
    # are rebuilt only when explicitly selected.
    --crates) CRATES_OVERRIDE="$2"; shift 2 ;;
    --build-profile) BUILD_PROFILE="$2"; shift 2 ;;
    --all) POPULATE_ALL=1; shift ;;
    *) ryeos_term_fail "unknown argument: $1"; exit 2 ;;
  esac
done

case "$BUILD_PROFILE" in
  release|latency-profiling) ;;
  *) ryeos_term_fail "invalid build profile: $BUILD_PROFILE (expected release or latency-profiling)"; exit 2 ;;
esac

# Refuse to build the whole workspace implicitly — that full release build is
# what exhausts memory. The caller must be explicit: name the packages that
# changed, or opt into the whole set with --all.
if [[ -n "$CRATES_OVERRIDE" && "$POPULATE_ALL" -eq 1 ]]; then
  ryeos_term_fail "--crates and --all are mutually exclusive build scopes"
  exit 2
fi
if [[ -z "$CRATES_OVERRIDE" && "$POPULATE_ALL" -ne 1 ]]; then
  ryeos_term_fail "refusing to rebuild the full bundle set implicitly"
  ryeos_term_info "pass --crates \"<Cargo package ...>\" for a focused rebuild, or --all to rebuild '$BUNDLE_SET'"
  exit 2
fi

if [[ -z "$KEY"   ]]; then ryeos_term_fail "--key <pem-path> is required"; exit 2; fi
if [[ -z "$OWNER" ]]; then ryeos_term_fail "--owner <label> is required"; exit 2; fi
if [[ ! -s "$KEY" ]]; then ryeos_term_fail "key file is empty or missing: $KEY"; exit 2; fi
if ! command -v openssl >/dev/null 2>&1; then ryeos_term_fail "openssl is required"; exit 2; fi
if ! command -v sha256sum >/dev/null 2>&1; then ryeos_term_fail "sha256sum is required"; exit 2; fi
if ! command -v base64 >/dev/null 2>&1; then ryeos_term_fail "base64 is required"; exit 2; fi
case "$BUNDLE_SET" in
  full|full-sandbox|central-host|standard|hosted-node|hosted-workflow|release-artifacts) ;;
  *) ryeos_term_fail "invalid --bundle-set: $BUNDLE_SET"; exit 2 ;;
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

  [[ -f "$file" ]] || { ryeos_term_fail "seed YAML missing: $file"; exit 2; }
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

assert_node_init_profile_inventory() {
  local directory="$INIT_SEED/profiles"
  local expected actual unsupported hardlinked profile

  if ! ryeos_validate_node_init_root "$INIT_SEED"; then
    ryeos_term_fail "source-root node init namespace is not closed"
    exit 2
  fi
  if [[ ! -d "$directory" || -L "$directory" ]]; then
    ryeos_term_fail "node init-profile directory is missing or unsafe: $directory"
    exit 2
  fi
  unsupported="$(find "$directory" -mindepth 1 -maxdepth 1 ! -type f -print -quit)"
  if [[ -n "$unsupported" ]]; then
    ryeos_term_fail "node init-profile inventory contains a link, directory, or special entry: $unsupported"
    exit 2
  fi
  hardlinked="$(find "$directory" -mindepth 1 -maxdepth 1 -type f -links +1 -print -quit)"
  if [[ -n "$hardlinked" ]]; then
    ryeos_term_fail "node init-profile inventory contains a multiply-linked file: $hardlinked"
    exit 2
  fi
  expected="$(ryeos_node_init_profile_file_names | sort)"
  actual="$(find "$directory" -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | sort)"
  if [[ "$actual" != "$expected" ]]; then
    ryeos_term_fail "node init-profile inventory does not match the authored contract"
    printf 'expected:\n%s\nactual:\n%s\n' "$expected" "$actual" >&2
    exit 2
  fi
  while IFS= read -r profile; do
    if ! ryeos_validate_node_init_profile \
      "$profile" "$directory/$profile.yaml"; then
      ryeos_term_fail "node init-profile contract is invalid: $profile"
      exit 2
    fi
  done < <(ryeos_node_init_profile_names)
}

# ── Setup ────────────────────────────────────────────────────────────

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
ryeos_term_info "target directory $TARGET"

CORE="$ROOT/bundles/core"
STD="$ROOT/bundles/standard"
WEB="$ROOT/bundles/web"
BROWSER="$ROOT/bundles/browser"
RYEOS_UI="$ROOT/bundles/ryeos-ui"
HOSTED_NODE="$ROOT/bundles/hosted-node"
CODEX="$ROOT/bundles/codex"
LOCAL_INFERENCE="$ROOT/bundles/local-inference"
SANDBOX_LINUX_BUBBLEWRAP="$ROOT/bundles/sandbox-linux-bubblewrap"
TVTA="$ROOT/bundles/tv-tracker-authoring"
SOURCE_ROOT_AI="$ROOT/bundles/.ai"
INIT_SEED="$SOURCE_ROOT_AI/node/init"
PUBLISHER_PUBKEY_RAW_B64="$(publisher_pubkey_raw_b64)"
PUBLISHER_FP="$(publisher_fingerprint)"
assert_node_init_profile_inventory

# Bin-managed bundles for this set come from the shared definition (central-auth
# is excluded — its Python implementation is tool support rather than a
# target-triple binary, and it is published unconditionally
# below, so it must never be cleaned/staged here).
BUNDLE_DIRS=()
while IFS= read -r _bundle_name; do
  BUNDLE_DIRS+=("$ROOT/bundles/$_bundle_name")
done < <(ryeos_bundle_set_bin_managed_names "$BUNDLE_SET")

STATIC_RELEASE="$TARGET/$TRIPLE/release"
PAYLOAD_STAGE=""
SIGN_APP_ROOT=""

# Exact executable closure staged by this bundle set. Each record is:
# bundle, binary, owning Cargo package, build class. Targeted population takes
# selected package outputs from Cargo and every unselected payload from the
# existing bundle generation—not from ambient target/ contents.
staged_payload_records_for_set() {
  printf '%s\t%s\t%s\t%s\n' \
    core rye-parser-yaml-document ryeos-handler-bins release \
    core rye-parser-yaml-header-document ryeos-handler-bins release \
    core rye-parser-regex-kv ryeos-handler-bins release \
    core rye-composer-identity ryeos-handler-bins release \
    core ryeos-core-tools ryeos-core-tools release \
    core ryeos-session-exec ryeos-session-exec static \
    core ryeos-worker-execution-launch-preparer ryeos-structured-session static \
    core ryeos-worker-execution-runtime ryeos-structured-session static
  case "$BUNDLE_SET" in
    full|full-sandbox|central-host|standard|hosted-workflow|release-artifacts)
      printf '%s\t%s\t%s\t%s\n' \
        standard ryeos-directive-runtime ryeos-directive-runtime release \
        standard ryeos-directive-launch-preparer ryeos-handler-bins release \
        standard ryeos-graph-runtime ryeos-graph-runtime release \
        standard ryeos-knowledge-runtime ryeos-knowledge-runtime release \
        standard rye-composer-extends-chain ryeos-handler-bins release \
        standard ryeos-graph-effective-validator ryeos-handler-bins release
      ;;
  esac
  case "$BUNDLE_SET" in
    full|full-sandbox|central-host|release-artifacts)
      printf '%s\t%s\t%s\t%s\n' web ryeos-web-tools ryeos-web-tools release
      ;;
  esac
  case "$BUNDLE_SET" in
    full|full-sandbox|release-artifacts)
      printf '%s\t%s\t%s\t%s\n' \
        ryeos-ui ryeos-tui ryeos-client-terminal release \
        ryeos-ui web ryeos-client-web release \
        browser ryeos-browser-tools ryeos-browser-tools release
      ;;
  esac
  case "$BUNDLE_SET" in
    full|full-sandbox|hosted-workflow|release-artifacts)
      printf '%s\t%s\t%s\t%s\n' \
        codex ryeos-structured-session-bridge ryeos-structured-session static
      ;;
  esac
}

package_selected() {
  local wanted="$1" selected
  for selected in "${pkgs[@]}"; do
    [[ "$selected" == "$wanted" ]] && return 0
  done
  return 1
}

payload_build_path() {
  local binary="$1" build_class="$2"
  case "$build_class" in
    release) printf '%s\n' "$TARGET/release/$binary" ;;
    static) printf '%s\n' "$STATIC_RELEASE/$binary" ;;
    *) ryeos_term_fail "unknown payload build class: $build_class"; exit 2 ;;
  esac
}

payload_source_path() {
  local bundle="$1" binary="$2" package="$3" build_class="$4"
  if package_selected "$package"; then
    payload_build_path "$binary" "$build_class"
  else
    printf '%s\n' "$ROOT/bundles/$bundle/.ai/bin/$TRIPLE/$binary"
  fi
}

require_staged_payloads() {
  local bundle binary package build_class path
  local -a missing=()
  while IFS=$'\t' read -r bundle binary package build_class; do
    path="$(payload_source_path "$bundle" "$binary" "$package" "$build_class")"
    [[ -x "$path" ]] || missing+=("$path")
  done < <(staged_payload_records_for_set)
  if (( ${#missing[@]} > 0 )); then
    ryeos_term_fail "bundle population is missing required staged artifacts"
    printf '    - %s\n' "${missing[@]}" >&2
    ryeos_term_info "select their Cargo packages with --crates or run --all"
    exit 2
  fi
}

cleanup_population() {
  local status="$1"
  ryeos_term_handle_exit "$status"
  [[ -z "$PAYLOAD_STAGE" ]] || rm -rf "$PAYLOAD_STAGE"
  [[ -z "$SIGN_APP_ROOT" ]] || rm -rf "$SIGN_APP_ROOT"
  return "$status"
}

materialize_staged_payloads() {
  local bundle binary package build_class source
  PAYLOAD_STAGE="$(mktemp -d)"
  trap 'cleanup_population "$?"' EXIT
  while IFS=$'\t' read -r bundle binary package build_class; do
    source="$(payload_source_path "$bundle" "$binary" "$package" "$build_class")"
    install -Dm755 "$source" "$PAYLOAD_STAGE/$bundle/$binary"
  done < <(staged_payload_records_for_set)
}

require_static_payload() {
  local path="$1"
  if readelf -l "$path" | grep -Eq '(^|[[:space:]])INTERP([[:space:]]|$)' \
      || readelf -d "$path" | grep -Eq 'NEEDED'; then
    ryeos_term_fail "admitted persistent-session payload is not fully static: $path"
    exit 2
  fi
}

prepare_bundle_trees() {
  local bundle_dir
  for bundle_dir in "${BUNDLE_DIRS[@]}"; do
    rm -rf "$bundle_dir/.ai/bin"
    rm -rf "$bundle_dir/.ai/objects"
    rm -rf "$bundle_dir/.ai/refs"
    rm -f  "$bundle_dir/PUBLISHER_TRUST.toml"
  done
  if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "full-sandbox" || "$BUNDLE_SET" == "release-artifacts" ]]; then
    rm -rf "$LOCAL_INFERENCE/.ai/bin"
    rm -rf "$LOCAL_INFERENCE/.ai/objects"
    rm -rf "$LOCAL_INFERENCE/.ai/refs"
    rm -f  "$LOCAL_INFERENCE/PUBLISHER_TRUST.toml"
  fi
  if [[ "$BUNDLE_SET" == "full-sandbox" ]]; then
    # Its separately authored binaries were preflighted and remain in place;
    # only its closed bundle graph is regenerated.
    rm -rf "$SANDBOX_LINUX_BUBBLEWRAP/.ai/objects"
    rm -rf "$SANDBOX_LINUX_BUBBLEWRAP/.ai/refs"
    rm -f  "$SANDBOX_LINUX_BUBBLEWRAP/PUBLISHER_TRUST.toml"
  fi
  for bundle_dir in "${BUNDLE_DIRS[@]}"; do
    mkdir -p "$bundle_dir/.ai/bin/$TRIPLE"
  done
}

# ── Build ────────────────────────────────────────────────────────────

# Cargo package list per bundle set (the default when --crates is not given).
case "$BUNDLE_SET" in
  full|full-sandbox|release-artifacts)
    pkgs=(lillux ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-web-tools ryeos-browser-tools \
          ryeos-client-terminal ryeos-client-web ryeos-structured-session)
    ;;
  central-host)
    pkgs=(lillux ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-web-tools ryeos-structured-session)
    ;;
  standard)
    pkgs=(lillux ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-structured-session)
    ;;
  hosted-workflow)
    pkgs=(lillux ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-structured-session)
    ;;
  hosted-node)
    pkgs=(lillux ryeosd ryeos-handler-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-structured-session)
    ;;
esac

# --crates overrides the build list (staging still copies all bundle binaries
# from target/release, so unbuilt ones must already exist there).
if [[ -n "$CRATES_OVERRIDE" ]]; then
  read -ra pkgs <<< "$CRATES_OVERRIDE"
fi

# Static admitted worker packages are built under an explicit target and must
# not also be compiled through the ordinary host package graph.
host_pkgs=()
build_static_session_exec=0
build_static_structured_session=0
for p in "${pkgs[@]}"; do
  case "$p" in
    ryeos-session-exec) build_static_session_exec=1 ;;
    ryeos-structured-session) build_static_structured_session=1 ;;
    *) host_pkgs+=("$p") ;;
  esac
done

build_args=()
for p in "${host_pkgs[@]}"; do build_args+=(-p "$p"); done
feature_args=()
if [[ "$BUILD_PROFILE" == latency-profiling ]]; then
  profiling_features=()
  for p in "${host_pkgs[@]}"; do
    case "$p" in
      ryeosd) profiling_features+=("ryeosd/latency-profiling") ;;
      ryeos-directive-runtime)
        profiling_features+=("ryeos-directive-runtime/latency-profiling")
        ;;
    esac
  done
  if (( ${#profiling_features[@]} > 0 )); then
    feature_list="$(IFS=,; printf '%s' "${profiling_features[*]}")"
    feature_args=(--features "$feature_list")
  else
    ryeos_term_fail "latency profiling requires ryeosd and/or ryeos-directive-runtime in --crates"
    exit 2
  fi
fi
jobs_args=()
[[ -n "$JOBS" ]] && jobs_args=(-j "$JOBS")

ryeos_term_begin PUBLISH "building $BUILD_PROFILE binaries${JOBS:+ (jobs=$JOBS)}"
if (( ${#host_pkgs[@]} > 0 )); then
  ryeos_term_update "building selected release binaries" "${host_pkgs[*]}"
  ryeos_term_suspend
  "$CARGO" build --release "${jobs_args[@]}" "${build_args[@]}" "${feature_args[@]}"
  ryeos_term_resume "selected release build complete"
else
  ryeos_term_update "retaining release binaries" "no host packages selected"
fi

# The admitted persistent-session bridge must bring no ambient loader/library
# closure into the worker realization. Rebuild only selected static packages;
# otherwise retain their existing exact artifacts. The explicit Cargo target
# keeps target RUSTFLAGS off host-built proc macros.
static_build_labels=()
(( build_static_session_exec == 1 )) && static_build_labels+=(ryeos-session-exec)
(( build_static_structured_session == 1 )) && static_build_labels+=(ryeos-structured-session)
if (( ${#static_build_labels[@]} > 0 )); then
  ryeos_term_update "building selected static worker binaries" "${static_build_labels[*]}"
  ryeos_term_suspend
  if (( build_static_session_exec == 1 )); then
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static" \
      "$CARGO" build --release --target "$TRIPLE" "${jobs_args[@]}" -p ryeos-session-exec
  fi
  if (( build_static_structured_session == 1 )); then
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static" \
      "$CARGO" build --release --target "$TRIPLE" "${jobs_args[@]}" -p ryeos-structured-session
  fi
  ryeos_term_resume "selected static worker build complete"
else
  ryeos_term_update "retaining static worker binaries" "no static packages selected"
fi

# Build first, then prove the complete artifact closure before deleting any
# generated source-bundle state. This makes a missing retained generation a
# read-only failure instead of a partial, unretryable publication.
require_staged_payloads
materialize_staged_payloads
require_static_payload "$PAYLOAD_STAGE/core/ryeos-session-exec"
require_static_payload "$PAYLOAD_STAGE/core/ryeos-worker-execution-launch-preparer"
require_static_payload "$PAYLOAD_STAGE/core/ryeos-worker-execution-runtime"
if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "full-sandbox" || "$BUNDLE_SET" == "hosted-workflow" || "$BUNDLE_SET" == "release-artifacts" ]]; then
  require_static_payload "$PAYLOAD_STAGE/codex/ryeos-structured-session-bridge"
fi
if [[ "$BUNDLE_SET" == "full-sandbox" ]]; then
  test -x "$SANDBOX_LINUX_BUBBLEWRAP/.ai/bin/$TRIPLE/bwrap" || {
    ryeos_term_fail "full-sandbox requires an explicitly built sandbox-linux-bubblewrap payload"
    ryeos_term_info "run ./bundles/sandbox-linux-bubblewrap/build-payload.sh before populate"
    exit 2
  }
  test -x "$SANDBOX_LINUX_BUBBLEWRAP/.ai/bin/$TRIPLE/ryeos-bubblewrap-adapter" || {
    ryeos_term_fail "full-sandbox requires an explicitly built sandbox-linux-bubblewrap adapter"
    ryeos_term_info "run ./bundles/sandbox-linux-bubblewrap/build-payload.sh before populate"
    exit 2
  }
fi
prepare_bundle_trees

# ── Stage binaries (only what each bundle owns) ──────────────────────

ryeos_term_update "installing exact bundle payload closure" "$BUNDLE_SET"
while IFS=$'\t' read -r payload_bundle payload_binary _payload_package _payload_class; do
  install -Dm755 \
    "$PAYLOAD_STAGE/$payload_bundle/$payload_binary" \
    "$ROOT/bundles/$payload_bundle/.ai/bin/$TRIPLE/$payload_binary"
done < <(staged_payload_records_for_set)

# ── Publish ──────────────────────────────────────────────────────────

ryeos_term_update "signing source-root seed data" "publisher $OWNER"
while IFS= read -r profile; do
  sign_seed_yaml "$INIT_SEED/profiles/$profile.yaml"
done < <(ryeos_node_init_profile_names)
write_seed_trust_doc

# Bundle publishing is an offline authoring operation. Use the maintainer
# binary directly rather than `ryeos publish`, because `publish` is no longer
# a lifecycle-local CLI verb on `next` and would otherwise route through a
# daemon/initialized-node dispatch path during Docker builds.
SIGN_APP_ROOT="$(mktemp -d)"
mkdir -p "$SIGN_APP_ROOT/.ai/config/keys/signing"
cp "$KEY" "$SIGN_APP_ROOT/.ai/config/keys/signing/private_key.pem"
chmod 0600 "$SIGN_APP_ROOT/.ai/config/keys/signing/private_key.pem"

ryeos_term_update "publishing core bundle" "signed manifests"
RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$CORE" \
  --registry-root "$CORE" \
  --owner "$OWNER" >/dev/null

# central-auth ships in the source tree and is discovered/parsed at init, so its
# manifest must stay current with the manifest schema. It depends only on core's
# tool + config kinds, so publish it right after core (now that core carries a
# published refs root) with core as its registry root.
ryeos_term_update "publishing central-auth bundle" "signed manifests"
RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$ROOT/bundles/central-auth" \
  --registry-root "$CORE" \
  --owner "$OWNER" >/dev/null

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "full-sandbox" || "$BUNDLE_SET" == "central-host" || "$BUNDLE_SET" == "standard" || "$BUNDLE_SET" == "hosted-workflow" || "$BUNDLE_SET" == "release-artifacts" ]]; then
  ryeos_term_update "publishing standard bundle" "signed manifests"
  # Standard contains its own kind schemas (directive, graph, knowledge) now.
  # Core kinds are needed for verifying handlers/tools, so we pass core as registry-root.
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$STD" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "full-sandbox" || "$BUNDLE_SET" == "central-host" || "$BUNDLE_SET" == "release-artifacts" ]]; then
  ryeos_term_update "publishing web bundle" "signed manifests"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$WEB" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "central-host" || "$BUNDLE_SET" == "release-artifacts" ]]; then
  # tv-tracker-authoring — source-only bundle (tool kind from core); ships the
  # operator context-doc author/read wrappers. No compiled binary of its own.
  ryeos_term_update "publishing tv-tracker-authoring bundle" "signed manifests"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$TVTA" \
    --registry-root "$CORE" \
    --registry-root "$STD" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "full-sandbox" || "$BUNDLE_SET" == "release-artifacts" ]]; then
  ryeos_term_update "publishing browser bundle" "signed manifests"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$BROWSER" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null

  ryeos_term_update "publishing ryeos-ui bundle" "signed manifests"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$RYEOS_UI" \
    --registry-root "$CORE" \
    --registry-root "$STD" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "full-sandbox" || "$BUNDLE_SET" == "hosted-node" || "$BUNDLE_SET" == "hosted-workflow" || "$BUNDLE_SET" == "release-artifacts" ]]; then
  ryeos_term_update "publishing hosted-node bundle" "signed manifests"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$HOSTED_NODE" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "full-sandbox" || "$BUNDLE_SET" == "hosted-workflow" || "$BUNDLE_SET" == "release-artifacts" ]]; then
  ryeos_term_update "publishing codex bundle" "signed manifests"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$CODEX" \
    --registry-root "$CORE" \
    --registry-root "$STD" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full-sandbox" ]]; then
  ryeos_term_update "publishing sandbox-linux-bubblewrap bundle" "signed manifests"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$SANDBOX_LINUX_BUBBLEWRAP" \
    --registry-root "$CORE" \
    --owner "$OWNER" >/dev/null
fi

if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "full-sandbox" || "$BUNDLE_SET" == "release-artifacts" ]]; then
  ryeos_term_update "publishing local-inference bundle" "signed manifests"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$LOCAL_INFERENCE" \
    --registry-root "$CORE" \
    --registry-root "$STD" \
    --owner "$OWNER" >/dev/null
fi

ryeos_term_end success "PUBLISH COMPLETE" "$BUNDLE_SET bundle set"
