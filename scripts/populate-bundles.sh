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
#   ./scripts/populate-bundles.sh --key <pem-path> --owner <label> [--bundle-set full|central-host|standard|standard-central-auth|local-inference|hosted-workflow|bundle-source|core|core-scheduling|core-workflow|core-knowledge|f5r-controller|release-authority|release-artifacts] (--crates "<package ...>" | --all) [--build-profile release|latency-profiling]
#
# Bundle sets:
#   full            core + central-auth + scheduling + knowledge + directive + graph +
#                   ryeos-docs + environment-qualification + web + browser + ryeos-ui +
#                   codex + opencode (default)
#   central-host    core + central-auth + scheduling + knowledge + directive + graph +
#                   ryeos-docs + web — workflow node plus the rye/web/search tool and
#                   app authentication
#   standard        core + scheduling + knowledge + directive + graph + ryeos-docs —
#                   scheduler/graph/directive node
#   standard-central-auth standard + optional web-app authentication
#   local-inference standard + local-inference — provider-neutral inference node
#   hosted-workflow standard + codex + opencode — hosted
#                   node that also runs scheduler/graph/directive and hosted workloads
#   bundle-source   core + bundle-source — catalog authority only
#   core            core — scheduler-free selection
#   core-scheduling core + scheduling — explicit schedule selection
#   core-workflow   core + graph — graph workflows without model providers
#   core-knowledge  core + knowledge — knowledge kind and runtime only
#   f5r-controller  core + graph + knowledge + codex + render-sandbox — remote Codex
#                   authoring controller without the scheduler or directive runtime
#   release-authority core + graph + environment-qualification + bundle-release —
#                   isolated release authoring node
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
  full|central-host|standard|standard-central-auth|local-inference|hosted-workflow|bundle-source|core|core-scheduling|core-workflow|core-knowledge|f5r-controller|release-authority|release-artifacts) ;;
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
  local tmp
  tmp="$(mktemp "${target}.tmp.XXXXXX")"
  cat > "$tmp" <<EOF
public_key = "ed25519:$PUBLISHER_PUBKEY_RAW_B64"
fingerprint = "$PUBLISHER_FP"
owner = "$OWNER"
EOF
  if cmp -s "$tmp" "$target"; then
    rm -f "$tmp"
    return 0
  fi
  chmod 0644 "$tmp"
  mv "$tmp" "$target"
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
KNOWLEDGE="$ROOT/bundles/knowledge"
DIRECTIVE="$ROOT/bundles/directive"
GRAPH="$ROOT/bundles/graph"
RYEOS_DOCS="$ROOT/bundles/ryeos-docs"
SCHEDULING="$ROOT/bundles/scheduling"
WEB="$ROOT/bundles/web"
BROWSER="$ROOT/bundles/browser"
RYEOS_UI="$ROOT/bundles/ryeos-ui"
CODEX="$ROOT/bundles/codex"
RENDER_SANDBOX="$ROOT/bundles/render-sandbox"
MODAL_SANDBOX="$ROOT/bundles/modal-sandbox"
OPENCODE="$ROOT/bundles/opencode"
LOCAL_INFERENCE="$ROOT/bundles/local-inference"
ENV_QUALIFICATION="$ROOT/bundles/environment-qualification"
RELEASE_BUNDLE="$ROOT/bundles/bundle-release"
BUNDLE_SOURCE="$ROOT/bundles/bundle-source"
SOURCE_ROOT_AI="$ROOT/bundles/.ai"
INIT_SEED="$SOURCE_ROOT_AI/node/init"
PUBLISHER_PUBKEY_RAW_B64="$(publisher_pubkey_raw_b64)"
PUBLISHER_FP="$(publisher_fingerprint)"
assert_node_init_profile_inventory

# Bin-managed bundles come from the explicit set. Optional central-auth is
# interpreted tool support, not a target-triple executable, and is published
# separately only when selected.
BUNDLE_DIRS=()
while IFS= read -r _bundle_name; do
  BUNDLE_DIRS+=("$ROOT/bundles/$_bundle_name")
done < <(ryeos_bundle_set_bin_managed_names "$BUNDLE_SET")

# Whether the selected bundle set contains one bundle.
bundle_in_set() {
  ryeos_bundle_set_names "$BUNDLE_SET" | grep -Fxq -- "$1"
}

STATIC_RELEASE="$TARGET/$TRIPLE/release"
PAYLOAD_STAGE=""
SIGN_APP_ROOT=""

# Exact executable closure staged by this bundle set. Each record is:
# bundle, binary, owning Cargo package, build class. Targeted population takes
# selected package outputs from Cargo and every unselected payload from the
# existing bundle generation—not from ambient target/ contents.
staged_payload_records_for_set() {
  "$ROOT/scripts/release/bundle-payload-ownership.py" \
    --repository-root "$ROOT" --bundle-set "$BUNDLE_SET" --format records || {
      ryeos_term_fail "signed payload ownership config was rejected"
      exit 2
    }
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
  local program_headers dynamic_entries
  # An absent marker is meaningful only after a successful ELF inspection.
  # Capture first: grep's early exit must not turn a producer SIGPIPE into
  # apparent evidence that the executable has no dynamic dependency.
  if ! program_headers="$(readelf -l "$path")" \
      || ! dynamic_entries="$(readelf -d "$path")"; then
    ryeos_term_fail "cannot inspect admitted persistent-session ELF payload: $path"
    exit 2
  fi
  if grep -Eq '(^|[[:space:]])INTERP([[:space:]]|$)' <<< "$program_headers" \
      || grep -Eq 'NEEDED' <<< "$dynamic_entries"; then
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
  if [[ "$BUNDLE_SET" == "full" || "$BUNDLE_SET" == "release-authority" || "$BUNDLE_SET" == "release-artifacts" ]]; then
    rm -rf "$LOCAL_INFERENCE/.ai/bin"
    rm -rf "$LOCAL_INFERENCE/.ai/objects"
    rm -rf "$LOCAL_INFERENCE/.ai/refs"
    rm -f  "$LOCAL_INFERENCE/PUBLISHER_TRUST.toml"
  fi
  for bundle_dir in "${BUNDLE_DIRS[@]}"; do
    mkdir -p "$bundle_dir/.ai/bin/$TRIPLE"
  done
}

# ── Build ────────────────────────────────────────────────────────────

# Cargo package list per bundle set (the default when --crates is not given).
case "$BUNDLE_SET" in
  release-authority)
    pkgs=(lillux ryeosd ryeos-graph-runtime \
          ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-structured-session \
          ryeos-lillux-isolation-adapter)
    ;;
  full)
    pkgs=(lillux ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-web-tools ryeos-browser-tools \
          ryeos-client-terminal ryeos-client-web ryeos-structured-session ryeos-lillux-isolation-adapter \
          ryeos-external-candidate-connector ryeos-codex-external-configuration \
          ryeos-external-guest-occurrence-owner \
          ryeos-external-guest-runtime-producer \
          ryeos-codex-guest-runtime-producer \
          ryeos-codex-runtime-verifier \
          ryeos-external-candidate-launcher ryeos-external-candidate-supervisor \
          ryeos-guest-wire-verifier)
    ;;
  release-artifacts)
    pkgs=(lillux ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-web-tools ryeos-browser-tools \
          ryeos-client-terminal ryeos-client-web ryeos-structured-session ryeos-lillux-isolation-adapter \
          ryeos-external-candidate-connector ryeos-codex-external-configuration \
          ryeos-external-guest-occurrence-owner \
          ryeos-external-guest-runtime-producer \
          ryeos-codex-guest-runtime-producer \
          ryeos-codex-runtime-verifier \
          ryeos-external-candidate-launcher ryeos-external-candidate-supervisor \
          ryeos-guest-wire-verifier \
          ryeos-external-guest-restoration-verifier ryeos-render-sandbox-lifecycle-adapter \
          modal-guest-launcher ryeos-modal-sandbox-lifecycle-adapter)
    ;;
  central-host)
    pkgs=(lillux ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-web-tools ryeos-structured-session \
          ryeos-lillux-isolation-adapter)
    ;;
  standard|standard-central-auth|local-inference)
    pkgs=(lillux ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-structured-session \
          ryeos-lillux-isolation-adapter)
    ;;
  hosted-workflow)
    pkgs=(lillux ryeosd ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-structured-session \
          ryeos-lillux-isolation-adapter ryeos-external-candidate-connector \
          ryeos-external-guest-occurrence-owner ryeos-codex-external-configuration \
          ryeos-external-guest-runtime-producer \
          ryeos-codex-guest-runtime-producer \
          ryeos-codex-runtime-verifier \
          ryeos-external-candidate-launcher ryeos-external-candidate-supervisor \
          ryeos-guest-wire-verifier)
    ;;
  f5r-controller)
    pkgs=(lillux ryeosd ryeos-graph-runtime ryeos-knowledge-runtime \
          ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools ryeos-session-exec ryeos-structured-session \
          ryeos-lillux-isolation-adapter ryeos-external-candidate-connector \
          ryeos-external-guest-occurrence-owner ryeos-codex-external-configuration \
          ryeos-external-guest-runtime-producer \
          ryeos-codex-guest-runtime-producer \
          ryeos-codex-runtime-verifier \
          ryeos-external-candidate-launcher ryeos-external-candidate-supervisor \
          ryeos-guest-wire-verifier \
          ryeos-external-guest-restoration-verifier ryeos-render-sandbox-lifecycle-adapter)
    ;;
  core-workflow)
    pkgs=(lillux ryeosd ryeos-graph-runtime ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools \
          ryeos-session-exec ryeos-structured-session ryeos-lillux-isolation-adapter)
    ;;
  core-knowledge)
    pkgs=(lillux ryeosd ryeos-knowledge-runtime ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools \
          ryeos-session-exec ryeos-structured-session ryeos-lillux-isolation-adapter)
    ;;
  bundle-source|core|core-scheduling)
    pkgs=(lillux ryeosd ryeos-handler-bins ryeos-parser-bins ryeos-cli ryeos-core-tools ryeos-session-exec \
          ryeos-structured-session ryeos-lillux-isolation-adapter)
    ;;
esac

# --crates overrides the build list. Bundle payload staging retains unselected
# artifacts from the existing exact bundle generation. The installed host is a
# single policy-reader generation, however: `ryeos init` executes node policy
# code linked into the CLI, the daemon consumes the resulting generation, and
# core-tools verifies installed bundles against it. Never publish one reader
# against stale counterparts.
if [[ -n "$CRATES_OVERRIDE" ]]; then
  read -ra pkgs <<< "$CRATES_OVERRIDE"
  selected_policy_reader=0
  for p in "${pkgs[@]}"; do
    case "$p" in
      ryeosd|ryeos-cli|ryeos-core-tools) selected_policy_reader=1 ;;
    esac
  done
  if (( selected_policy_reader == 1 )); then
    for required in ryeosd ryeos-cli ryeos-core-tools; do
      present=0
      for p in "${pkgs[@]}"; do
        [[ "$p" == "$required" ]] && present=1
      done
      (( present == 1 )) || pkgs+=("$required")
    done
  fi
fi

# Publication builds must use the reviewed lockfile. A required resolution
# change is reconciled before construction rather than signed implicitly.
# Static admitted worker packages are built under an explicit target and must
# not also be compiled through the ordinary host package graph.
host_pkgs=()
build_static_session_exec=0
build_static_structured_session=0
build_static_lillux_isolation_adapter=0
build_static_external_candidate_connector=0
build_static_external_guest_occurrence_owner=0
build_static_external_candidate_launcher=0
build_static_external_candidate_supervisor=0
build_static_external_guest_restoration_verifier=0
build_static_external_guest_runtime_producer=0
build_static_render_sandbox_lifecycle_adapter=0
build_static_modal_guest_launcher=0
build_static_modal_sandbox_lifecycle_adapter=0
build_static_codex_external_configuration=0
build_static_codex_guest_runtime_producer=0
build_static_codex_runtime_verifier=0
build_static_guest_wire_verifier=0
for p in "${pkgs[@]}"; do
  case "$p" in
    ryeos-session-exec) build_static_session_exec=1 ;;
    ryeos-structured-session) build_static_structured_session=1 ;;
    ryeos-lillux-isolation-adapter) build_static_lillux_isolation_adapter=1 ;;
    ryeos-external-candidate-connector) build_static_external_candidate_connector=1 ;;
    ryeos-external-guest-occurrence-owner) build_static_external_guest_occurrence_owner=1 ;;
    ryeos-external-candidate-launcher) build_static_external_candidate_launcher=1 ;;
    ryeos-external-candidate-supervisor) build_static_external_candidate_supervisor=1 ;;
    ryeos-external-guest-restoration-verifier) build_static_external_guest_restoration_verifier=1 ;;
    ryeos-external-guest-runtime-producer) build_static_external_guest_runtime_producer=1 ;;
    ryeos-render-sandbox-lifecycle-adapter) build_static_render_sandbox_lifecycle_adapter=1 ;;
    modal-guest-launcher) build_static_modal_guest_launcher=1 ;;
    ryeos-modal-sandbox-lifecycle-adapter) build_static_modal_sandbox_lifecycle_adapter=1 ;;
    ryeos-codex-external-configuration) build_static_codex_external_configuration=1 ;;
    ryeos-codex-guest-runtime-producer) build_static_codex_guest_runtime_producer=1 ;;
    ryeos-codex-runtime-verifier) build_static_codex_runtime_verifier=1 ;;
    ryeos-guest-wire-verifier) build_static_guest_wire_verifier=1 ;;
    *) host_pkgs+=("$p") ;;
  esac
done

build_args=()
for p in "${host_pkgs[@]}"; do build_args+=(-p "$p"); done
feature_args=()
selected_features=()
if [[ "$BUILD_PROFILE" == latency-profiling ]]; then
  for p in "${host_pkgs[@]}"; do
    case "$p" in
      ryeosd) selected_features+=("ryeosd/latency-profiling") ;;
      ryeos-directive-runtime)
        selected_features+=("ryeos-directive-runtime/latency-profiling")
        ;;
    esac
  done
  if (( ${#selected_features[@]} == 0 )); then
    ryeos_term_fail "latency profiling requires ryeosd and/or ryeos-directive-runtime in --crates"
    exit 2
  fi
fi
for p in "${host_pkgs[@]}"; do
  composition_feature="$(ryeos_bundle_set_cargo_features "$BUNDLE_SET" "$p")" || {
    ryeos_term_fail "cannot select native composition for $BUNDLE_SET/$p"
    exit 2
  }
  if [[ -n "$composition_feature" ]]; then
    selected_features+=("$composition_feature")
  fi
done
if (( ${#selected_features[@]} > 0 )); then
  feature_list="$(IFS=,; printf '%s' "${selected_features[*]}")"
  feature_args=(--features "$feature_list")
fi
jobs_args=()
[[ -n "$JOBS" ]] && jobs_args=(-j "$JOBS")

ryeos_term_begin PUBLISH "building $BUILD_PROFILE binaries${JOBS:+ (jobs=$JOBS)}"
if (( ${#host_pkgs[@]} > 0 )); then
  ryeos_term_update "building selected release binaries" "${host_pkgs[*]}"
  ryeos_term_suspend
  ryeos_term_info "host release build: node, CLI, runtimes, tools, and clients"
  "$CARGO" build --release --locked "${jobs_args[@]}" "${build_args[@]}" "${feature_args[@]}"
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
(( build_static_lillux_isolation_adapter == 1 )) && static_build_labels+=(ryeos-lillux-isolation-adapter)
(( build_static_external_candidate_connector == 1 )) && static_build_labels+=(ryeos-external-candidate-connector)
(( build_static_external_guest_occurrence_owner == 1 )) && static_build_labels+=(ryeos-external-guest-occurrence-owner)
(( build_static_external_candidate_launcher == 1 )) && static_build_labels+=(ryeos-external-candidate-launcher)
(( build_static_external_candidate_supervisor == 1 )) && static_build_labels+=(ryeos-external-candidate-supervisor)
(( build_static_external_guest_restoration_verifier == 1 )) && static_build_labels+=(ryeos-external-guest-restoration-verifier)
(( build_static_external_guest_runtime_producer == 1 )) && static_build_labels+=(ryeos-external-guest-runtime-producer)
(( build_static_render_sandbox_lifecycle_adapter == 1 )) && static_build_labels+=(ryeos-render-sandbox-lifecycle-adapter)
(( build_static_modal_guest_launcher == 1 )) && static_build_labels+=(modal-guest-launcher)
(( build_static_modal_sandbox_lifecycle_adapter == 1 )) && static_build_labels+=(ryeos-modal-sandbox-lifecycle-adapter)
(( build_static_codex_external_configuration == 1 )) && static_build_labels+=(ryeos-codex-external-configuration)
(( build_static_codex_guest_runtime_producer == 1 )) && static_build_labels+=(ryeos-codex-guest-runtime-producer)
(( build_static_codex_runtime_verifier == 1 )) && static_build_labels+=(ryeos-codex-runtime-verifier)
(( build_static_guest_wire_verifier == 1 )) && static_build_labels+=(ryeos-guest-wire-verifier)
if (( ${#static_build_labels[@]} > 0 )); then
  ryeos_term_update "building selected static worker binaries" "${static_build_labels[*]}"
  ryeos_term_suspend
  # These packages share the same target and flags. One Cargo graph reuses
  # their common dependencies and runs publication only after all succeed.
  static_build_args=()
  for package in "${static_build_labels[@]}"; do
    static_build_args+=(-p "$package")
  done
  ryeos_term_info "static worker build: ${static_build_labels[*]}"
  RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static" \
    "$CARGO" build --release --locked --target "$TRIPLE" "${jobs_args[@]}" "${static_build_args[@]}"
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
require_static_payload "$PAYLOAD_STAGE/core/ryeos-structured-session-bridge"
require_static_payload "$PAYLOAD_STAGE/core/ryeos-lillux-isolation-adapter"
if bundle_in_set codex; then
  require_static_payload "$PAYLOAD_STAGE/codex/codex-runtime-verifier"
  require_static_payload "$PAYLOAD_STAGE/codex/ryeos-external-candidate-connector"
  require_static_payload "$PAYLOAD_STAGE/codex/ryeos-external-guest-occurrence-owner"
  require_static_payload "$PAYLOAD_STAGE/codex/ryeos-external-guest-runtime-producer"
  require_static_payload "$PAYLOAD_STAGE/codex/ryeos-codex-external-configuration"
  require_static_payload "$PAYLOAD_STAGE/codex/ryeos-codex-guest-runtime-producer"
  require_static_payload "$PAYLOAD_STAGE/codex/ryeos-external-candidate-launcher"
  require_static_payload "$PAYLOAD_STAGE/codex/ryeos-external-candidate-supervisor"
  require_static_payload "$PAYLOAD_STAGE/codex/ryeos-guest-wire-verifier"
fi
if bundle_in_set render-sandbox; then
  require_static_payload "$PAYLOAD_STAGE/render-sandbox/ryeos-external-guest-restoration-verifier"
  require_static_payload "$PAYLOAD_STAGE/render-sandbox/ryeos-render-sandbox-lifecycle-adapter"
fi
if bundle_in_set modal-sandbox; then
  require_static_payload "$PAYLOAD_STAGE/modal-sandbox/ryeos-modal-guest-launcher"
  require_static_payload "$PAYLOAD_STAGE/modal-sandbox/ryeos-modal-sandbox-lifecycle-adapter"
fi

# Payloads one bundle builds and other bundles carry are installed from that
# single build; the staged record holds the one digest every copy must match.
shared_payload_tool() {
  "$ROOT/scripts/release/shared-static-payloads.py" --repository-root "$ROOT" "$@" || {
    ryeos_term_fail "shared static payload staging was rejected"
    exit 2
  }
}
shared_payload_tool records --bundle-set "$BUNDLE_SET" >/dev/null
shared_payload_tool stage --bundle-set "$BUNDLE_SET" --stage-dir "$PAYLOAD_STAGE"
while IFS=$'\t' read -r shared_bundle shared_binary; do
  require_static_payload "$PAYLOAD_STAGE/$shared_bundle/$shared_binary"
done < <(shared_payload_tool records --bundle-set "$BUNDLE_SET")
prepare_bundle_trees

# ── Stage binaries (only what each bundle owns) ──────────────────────

ryeos_term_update "installing exact bundle payload closure" "$BUNDLE_SET"
while IFS=$'\t' read -r payload_bundle payload_binary _payload_package _payload_class; do
  install -Dm755 \
    "$PAYLOAD_STAGE/$payload_bundle/$payload_binary" \
    "$ROOT/bundles/$payload_bundle/.ai/bin/$TRIPLE/$payload_binary"
done < <(staged_payload_records_for_set)
while IFS=$'\t' read -r shared_bundle shared_binary; do
  install -Dm755 \
    "$PAYLOAD_STAGE/$shared_bundle/$shared_binary" \
    "$ROOT/bundles/$shared_bundle/.ai/bin/$TRIPLE/$shared_binary"
done < <(shared_payload_tool records --bundle-set "$BUNDLE_SET")

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

# Application authentication is published only for explicit app compositions.
# Presence in the repository never makes it a baseline dependency.
while IFS= read -r selected_bundle; do
  if [[ "$selected_bundle" == central-auth ]]; then
    ryeos_term_update "publishing central-auth bundle" "signed manifests"
    RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$ROOT/bundles/central-auth" \
      --registry-root "$CORE" \
      --owner "$OWNER" >/dev/null
    break
  fi
done < <(ryeos_bundle_set_names "$BUNDLE_SET")

# Scheduling owns service/command content, requires only Core kinds, and has
# no separately staged binary. Publish it according to the shared membership
# selector rather than inferring it from Standard's presence.
while IFS= read -r selected_bundle; do
  if [[ "$selected_bundle" == scheduling ]]; then
    ryeos_term_update "publishing scheduling bundle" "signed manifests"
    RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$SCHEDULING" \
      --registry-root "$CORE" \
      --owner "$OWNER" >/dev/null
    break
  fi
done < <(ryeos_bundle_set_names "$BUNDLE_SET")

# Each remaining bundle is published when the selected set contains it. The
# registry roots are exactly the bundles providing the kinds it uses.
publish_bundle() {
  local name="$1" dir="$2" label="$3"
  shift 3
  local roots=(--registry-root "$CORE") root
  for root in "$@"; do
    roots+=(--registry-root "$root")
  done
  bundle_in_set "$name" || return 0
  ryeos_term_update "publishing $name bundle" "$label"
  RYEOS_APP_ROOT="$SIGN_APP_ROOT" "$PAYLOAD_STAGE/core/ryeos-core-tools" build "$dir" \
    "${roots[@]}" \
    --owner "$OWNER" >/dev/null
}

publish_bundle knowledge "$KNOWLEDGE" "signed manifests"
publish_bundle graph "$GRAPH" "signed manifests"
publish_bundle directive "$DIRECTIVE" "signed manifests" "$KNOWLEDGE"
publish_bundle ryeos-docs "$RYEOS_DOCS" "signed manifests" "$KNOWLEDGE"
publish_bundle environment-qualification "$ENV_QUALIFICATION" "signed qualification policies"
publish_bundle web "$WEB" "signed manifests"
publish_bundle browser "$BROWSER" "signed manifests"
publish_bundle ryeos-ui "$RYEOS_UI" "signed manifests" "$DIRECTIVE" "$GRAPH" "$KNOWLEDGE"
publish_bundle codex "$CODEX" "signed manifests" "$GRAPH" "$KNOWLEDGE"
publish_bundle render-sandbox "$RENDER_SANDBOX" "signed lifecycle profile" "$GRAPH"
publish_bundle modal-sandbox "$MODAL_SANDBOX" "signed lifecycle profile" "$GRAPH"
publish_bundle opencode "$OPENCODE" "signed manifests" "$KNOWLEDGE"
publish_bundle local-inference "$LOCAL_INFERENCE" "signed manifests" "$DIRECTIVE" "$GRAPH" "$KNOWLEDGE"
publish_bundle bundle-release "$RELEASE_BUNDLE" "signed release-authoring contracts" "$GRAPH"
publish_bundle bundle-source "$BUNDLE_SOURCE" "signed catalog authority contracts"

if [[ -s "$PAYLOAD_STAGE/shared-payloads.json" && "$(<"$PAYLOAD_STAGE/shared-payloads.json")" != "{}" ]]; then
  ryeos_term_update "verifying shared static payloads" "one build, one hash"
  shared_payload_tool verify --triple "$TRIPLE" --stage-dir "$PAYLOAD_STAGE"
fi

ryeos_term_end success "PUBLISH COMPLETE" "$BUNDLE_SET bundle set"
