#!/bin/bash
# Quick-start a local ryeos-node.
#
# Usage:
#   ./run.sh                              # defaults to ~/.ryeos-node
#   CAS_BASE_PATH=/data/cas ./run.sh      # custom CAS path
#   PORT=9000 ./run.sh                    # custom port
set -e

CAS="${CAS_BASE_PATH:-$HOME/.ryeos-node}"
export CAS_BASE_PATH="$CAS"

python -m ryeos_node.init "$CAS"
exec uvicorn ryeos_node.server:app --host 0.0.0.0 --port "${PORT:-8000}"
