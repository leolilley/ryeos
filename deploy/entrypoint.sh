#!/usr/bin/env bash
# Entrypoint for ryeosd-full container.
#
# Always runs `ryeos init` on every boot. Init is idempotent — on first boot
# it creates keys, trust, and lays down bundles; on subsequent boots it
# re-verifies and re-copies to bring bundles up to date with the image.
#
# Both system space (/data/core) and user space (/data/user) live on the
# persistent /data volume, so operator trust and signing keys survive
# container redeploys.
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
for f in /opt/ryeos/core/PUBLISHER_TRUST.toml /opt/ryeos/standard/PUBLISHER_TRUST.toml; do
  if [ -f "$f" ]; then
    TRUST_ARGS+=(--trust-file "$f")
  fi
done

ryeos init \
  --system-space-dir /data/core \
  --user-root /data/user \
  --core-source /opt/ryeos/core \
  --standard-source /opt/ryeos/standard \
  "${TRUST_ARGS[@]}"

echo "[entrypoint] init complete, starting daemon"
# `--init-if-missing` lets the daemon's own bootstrap fill in artifacts
# `ryeos init` doesn't produce (e.g. public-identity.json, vault keypair).
# Idempotent — no-op when already written.
exec ryeosd \
  --init-if-missing \
  --system-space-dir /data/core \
  --bind "[::]:${PORT:-8000}"
