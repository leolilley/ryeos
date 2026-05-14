#!/usr/bin/env bash
# One-command developer bootstrap: build, publish bundles, init, and start daemon.
#
# Usage:
#   git clone <repo>
#   cd ryeos
#   ./scripts/dev-up.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOCAL="$ROOT/.local/ryeos"

# Build + publish bundles using dev key
"$ROOT/scripts/populate-bundles.sh" \
  --key "$ROOT/.dev-keys/PUBLISHER_DEV.pem" \
  --owner ryeos-dev

# Init operator state using dev trust
"$ROOT/target/release/ryeos" init \
  --system-space-dir "$LOCAL" \
  --source "$ROOT/ryeos-bundles" \
  --trust-file "$ROOT/.dev-keys/PUBLISHER_DEV_TRUST.toml"

# Start daemon
echo ""
echo "[dev-up] starting daemon at $LOCAL"
exec "$ROOT/target/release/ryeosd" --system-space-dir "$LOCAL"
