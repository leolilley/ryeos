#!/usr/bin/env bash
# Entrypoint for ryeosd-full container.
#
# Always runs `ryeos init` on every boot. Init is idempotent — on first boot
# it creates keys, trust, and lays down bundles; on subsequent boots it
# re-verifies and re-copies to bring bundles up to date with the image.
#
# App root (/data/app) lives on the persistent /data volume, so operator
# trust, signing keys, node identity, and runtime state survive redeploys.
#
# If init fails the container exits immediately — never start ryeosd against
# an unverified state.

set -euo pipefail

echo "[entrypoint] running ryeos init"
mkdir -p /data

# Pin the publisher who signed the baked bundles. For prod images this is
# the official publisher (already trusted via OFFICIAL_PUBLISHER_*); for
# dev images this is whichever key built the image. Either way, init
# needs the bundle's signer trusted before it preflights bundle items.
TRUST_ARGS=()
for f in /opt/ryeos/.ai/PUBLISHER_TRUST.toml /opt/ryeos/*/PUBLISHER_TRUST.toml; do
  if [ -f "$f" ]; then
    TRUST_ARGS+=(--trust-file "$f")
  fi
done

ryeos init \
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
