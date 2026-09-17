#!/usr/bin/env bash
# Fail-closed entry for the hard-contained workflow image. The external
# administrator has already initialized /data/app with the signed contained
# profile and prepared the protected binding. This image neither provisions
# host authority nor falls back to an ordinary daemon launch.

set -euo pipefail

readonly BINDING=/run/ryeos/host-runtime.json

if [[ $# -ne 0 ]]; then
  echo "contained-workflow entrypoint accepts no command override" >&2
  exit 2
fi
if [[ ! -f "$BINDING" || -L "$BINDING" ]]; then
  echo "contained-workflow requires the administrator-prepared protected binding at $BINDING" >&2
  exit 1
fi

exec /usr/local/bin/ryeosd host-runtime --binding "$BINDING"
