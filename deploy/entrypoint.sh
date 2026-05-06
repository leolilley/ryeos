#!/usr/bin/env bash
# Entrypoint for ryeosd-full container.
#
# 1. First boot only: copies baked bundles from /opt to /data (the persistent
#    volume). Subsequent boots no-op because /data/core/.ai already exists.
# 2. Always: runs --init-if-missing to generate node identity, vault, default
#    config if not present. Idempotent.
# 3. Always: hands off to ryeosd bound to $PORT (default 8000).

set -euo pipefail

if [ ! -d /data/core/.ai ]; then
  echo "[entrypoint] first boot: seeding /data from /opt/ryeos"
  mkdir -p /data
  cp -a /opt/ryeos/core     /data/core
  cp -a /opt/ryeos/standard /data/standard
fi

ryeosd --init-if-missing --system-space-dir /data/core

exec ryeosd \
  --system-space-dir /data/core \
  --bind "0.0.0.0:${PORT:-8000}"
