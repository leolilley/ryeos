#!/usr/bin/env bash
# Entrypoint for ryeosd-full container.
#
# Always runs `ryeos init --non-interactive` on every boot. Init is idempotent — on first boot
# it creates keys, trust, and lays down bundles; on subsequent boots it
# re-verifies and re-copies to bring bundles up to date with the image.
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

main() {
  echo "[entrypoint] running ryeos init --non-interactive"
  mkdir -p /data

  collect_baked_publisher_trust_args /opt/ryeos

  ryeos init \
    --non-interactive \
    --app-root /data/app \
    --source /opt/ryeos \
    "${TRUST_ARGS[@]}"

  echo "[entrypoint] init complete, starting daemon"
  # Daemon bootstrap auto-inits any artifacts `ryeos init` doesn't produce
  # (e.g. public-identity.json, vault keypair). Idempotent — no-op when
  # already written.
  exec ryeosd \
    --app-root /data/app \
    --bind "[::]:${PORT:-8000}"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
