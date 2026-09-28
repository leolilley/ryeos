#!/usr/bin/env bash
set -euo pipefail

config="${1:?usage: validate-native-deployment-coordinates.sh <deployment.env>}"
[[ -f "$config" && ! -L "$config" ]] || {
  echo "deployment coordinate file must be one regular file" >&2
  exit 1
}

set -a
# shellcheck disable=SC1090
source "$config"
set +a

[[ "${RYEOS_SUBSTRATE_IMAGE:-}" =~ ^ghcr\.io/[a-z0-9._/-]+@sha256:[0-9a-f]{64}$ ]] || {
  echo "RYEOS_SUBSTRATE_IMAGE must be an immutable GHCR digest" >&2
  exit 1
}
[[ "${RYEOS_SUBSTRATE_PROTOCOL:-}" =~ ^[1-9][0-9]*$ ]] || {
  echo "RYEOS_SUBSTRATE_PROTOCOL must be a nonzero integer" >&2
  exit 1
}
for name in \
  RYEOS_NODE_BUNDLE_SELECTION_HASH \
  RYEOS_NODE_BUNDLE_SELECTION_AUTHORIZATION_HASH \
  RYEOS_BUNDLE_PUBLICATION_POLICY_SECTION_DIGEST \
  RYEOS_NODE_POLICY_GENERATION_DIGEST
do
  [[ "${!name:-}" =~ ^[0-9a-f]{64}$ ]] || {
    echo "$name must be one canonical sha256 hash" >&2
    exit 1
  }
done
[[ "${RYEOS_CATALOG_NAMESPACE:-}" =~ ^[a-z][a-z0-9_-]*$ ]] || {
  echo "RYEOS_CATALOG_NAMESPACE must be canonical" >&2
  exit 1
}
