#!/usr/bin/env bash
# Entrypoint for ryeosd-full container.
#
# Ordinary mode seeds an absent installed generation exactly once. Subsequent
# boots never reconcile installed bundles from image contents: native bundle
# selection and stopped-node activation own those changes. External
# host-runtime mode consumes an administrator-created
# protected binding and does not run image-owned initialization.
#
# App root (/data/app) lives on the persistent /data volume, so operator
# trust, signing keys, node identity, and runtime state survive redeploys.
#
# If init fails the container exits immediately — never start ryeosd against
# an unverified state.

set -euo pipefail

# Fill TRUST_ARGS only after an operator explicitly opts in to trusting
# publisher documents baked into the image. Release images need no trust
# arguments: `ryeos init` pins the official publisher from key bytes compiled
# into the CLI. A locally built image signed by a development/custom key must
# instead be started with RYEOS_TRUST_BAKED_PUBLISHERS=1.
collect_baked_publisher_trust_args() {
  local source_dir="$1"
  local trust_file
  local trust_files=()

  TRUST_ARGS=()
  case "${RYEOS_TRUST_BAKED_PUBLISHERS:-0}" in
    0|"")
      return 0
      ;;
    1)
      if [[ -f "$source_dir/.ai/PUBLISHER_TRUST.toml" ]]; then
        trust_files+=("$source_dir/.ai/PUBLISHER_TRUST.toml")
      fi
      for trust_file in "$source_dir"/*/PUBLISHER_TRUST.toml; do
        [[ -f "$trust_file" ]] && trust_files+=("$trust_file")
      done
      if [[ ${#trust_files[@]} -eq 0 ]]; then
        echo "[entrypoint] RYEOS_TRUST_BAKED_PUBLISHERS=1, but no baked publisher trust documents were found under $source_dir" >&2
        return 1
      fi
      echo "[entrypoint] explicitly trusting ${#trust_files[@]} baked publisher document(s)"
      for trust_file in "${trust_files[@]}"; do
        TRUST_ARGS+=(--trust-file "$trust_file")
      done
      ;;
    *)
      echo "[entrypoint] invalid RYEOS_TRUST_BAKED_PUBLISHERS value; use 0 (default) or 1" >&2
      return 1
      ;;
  esac
}

container_bind() {
  printf '[::]:%s' "${PORT:-8000}"
}

# Select exactly one daemon entry contract. An external host-runtime binding
# means the administrator/environment builder has already initialized the
# bound app-root generation and delegated its process-scope controller. The
# ordinary container entry must neither rewrite that generation as root nor
# silently fall back to direct daemon execution.
build_ryeos_daemon_args() {
  local app_root="$1"

  DAEMON_ARGS=()
  if [[ -n "${RYEOS_HOST_RUNTIME_BINDING:-}" ]]; then
    [[ "$RYEOS_HOST_RUNTIME_BINDING" = /* ]] || {
      echo "[entrypoint] RYEOS_HOST_RUNTIME_BINDING must be an absolute protected path" >&2
      return 1
    }
    DAEMON_ARGS=(host-runtime --binding "$RYEOS_HOST_RUNTIME_BINDING")
    return 0
  fi
  DAEMON_ARGS=(--app-root "$app_root")
}

# Build the lifecycle argument vector without knowing any distribution or
# provider name. Every image must set the one generic init-profile variable. It is
# first-publication authority only: an absent generation receives the exact
# mapped profile, while any present generation occupant is preserved and left to
# Rust's signed complete-generation validation. A malformed or partial occupant
# therefore fails; packaging never falls back to the profile.
build_ryeos_init_args() {
  local source_dir="$1"
  local app_root="$2"
  local bind="$3"
  local policy_generation="$app_root/.ai/node/policies"

  [[ -n "${RYEOS_INIT_NODE_PROFILE:-}" ]] || {
    echo "[entrypoint] RYEOS_INIT_NODE_PROFILE is required" >&2
    return 1
  }
  [[ "${RYEOS_SUBSTRATE_IMAGE:-}" =~ @sha256:[0-9a-f]{64}$ ]] || {
    echo "[entrypoint] RYEOS_SUBSTRATE_IMAGE must be pinned by digest" >&2
    return 1
  }
  [[ "${RYEOS_SUBSTRATE_PROTOCOL:-}" =~ ^[1-9][0-9]*$ ]] || {
    echo "[entrypoint] RYEOS_SUBSTRATE_PROTOCOL must be a nonzero integer" >&2
    return 1
  }
  INIT_ARGS=(
    init
    --non-interactive
    --app-root "$app_root"
    --source "$source_dir"
    --bind "$bind"
    --substrate-image-digest "${RYEOS_SUBSTRATE_IMAGE##*@}"
    --substrate-protocol "$RYEOS_SUBSTRATE_PROTOCOL"
  )
  case "${RYEOS_RESET_NODE_POLICY_GENERATION:-0}" in
    0|"")
      if [[ ! -e "$policy_generation" && ! -L "$policy_generation" ]]; then
        INIT_ARGS+=(--node-profile "$RYEOS_INIT_NODE_PROFILE")
      else
        echo "[entrypoint] preserving existing signed node policy generation"
      fi
      ;;
    1)
      if [[ -e "$policy_generation" || -L "$policy_generation" ]]; then
        echo "[entrypoint] explicitly replacing obsolete signed node policy generation"
        INIT_ARGS+=(
          --node-profile "$RYEOS_INIT_NODE_PROFILE"
          --replace-node-policy-generation
          --confirm-node-policy-generation-replacement
        )
      else
        INIT_ARGS+=(--node-profile "$RYEOS_INIT_NODE_PROFILE")
      fi
      ;;
    *)
      echo "[entrypoint] invalid RYEOS_RESET_NODE_POLICY_GENERATION value; use 0 (default) or 1" >&2
      return 1
      ;;
  esac
}

build_execution_history_schema_cut_args() {
  local app_root="$1"
  local cut="${RYEOS_EXECUTION_HISTORY_SCHEMA_CUT:-}"

  EXECUTION_HISTORY_SCHEMA_CUT_ARGS=()
  if [[ -z "$cut" ]]; then
    return 0
  fi
  if [[ ! "$cut" =~ ^([0-9]+):([0-9]+)$ ]] || [[ "${BASH_REMATCH[1]}" == "${BASH_REMATCH[2]}" ]]; then
    echo "[entrypoint] invalid RYEOS_EXECUTION_HISTORY_SCHEMA_CUT value; use exact distinct epochs FROM:TO" >&2
    return 1
  fi
  EXECUTION_HISTORY_SCHEMA_CUT_ARGS=(
    node reset execution-history
    --app-root "$app_root"
    --confirm
    --schema-cut-from "${BASH_REMATCH[1]}"
    --schema-cut-to "${BASH_REMATCH[2]}"
  )
}

main() {
  local effective_bind
  effective_bind="$(container_bind)"

  build_ryeos_daemon_args /data/app
  if [[ "${DAEMON_ARGS[0]}" == "host-runtime" ]]; then
    echo "[entrypoint] starting with protected external host-runtime authority"
    exec ryeosd "${DAEMON_ARGS[@]}"
  fi

  mkdir -p /data
  if [[ ! -e /data/app/.ai/bundles && ! -L /data/app/.ai/bundles ]]; then
    echo "[entrypoint] installed bundle generation absent; seeding from substrate image"
    collect_baked_publisher_trust_args /opt/ryeos
    build_ryeos_init_args /opt/ryeos /data/app "$effective_bind"
    ryeos "${INIT_ARGS[@]}" "${TRUST_ARGS[@]}"
  else
    if [[ "${RYEOS_RESET_NODE_POLICY_GENERATION:-0}" != 0 && -n "${RYEOS_RESET_NODE_POLICY_GENERATION:-}" ]]; then
      echo "[entrypoint] node-policy replacement cannot use the first-boot image seed; use an authorized offline operation" >&2
      return 1
    fi
    echo "[entrypoint] preserving installed bundle generation; image seed is first-boot only"
  fi

  build_execution_history_schema_cut_args /data/app
  if [[ ${#EXECUTION_HISTORY_SCHEMA_CUT_ARGS[@]} -gt 0 ]]; then
    echo "[entrypoint] applying exact idempotent execution-history schema cut ${RYEOS_EXECUTION_HISTORY_SCHEMA_CUT}"
    ryeos "${EXECUTION_HISTORY_SCHEMA_CUT_ARGS[@]}"
  fi

  echo "[entrypoint] bootstrap check complete, starting daemon"
  # Daemon bootstrap auto-inits any artifacts `ryeos init` doesn't produce
  # (e.g. public-identity.json, vault keypair). Idempotent — no-op when
  # already written.
  exec ryeosd "${DAEMON_ARGS[@]}"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
