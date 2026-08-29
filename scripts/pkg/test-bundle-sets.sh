#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$ROOT/scripts/pkg/bundle-sets.sh"

mapfile -t full < <(ryeos_bundle_set_names full)
mapfile -t sandbox < <(ryeos_bundle_set_names full-sandbox)
mapfile -t full_bin_managed < <(ryeos_bundle_set_bin_managed_names full)
mapfile -t sandbox_bin_managed < <(ryeos_bundle_set_bin_managed_names full-sandbox)

contains() {
  local needle="$1"
  shift
  local value
  for value in "$@"; do
    [[ "$value" == "$needle" ]] && return 0
  done
  return 1
}

for set_name in full full-sandbox central-host standard hosted-node hosted-workflow; do
  mapfile -t members < <(ryeos_bundle_set_names "$set_name")
  contains central-auth "${members[@]}"
done

contains local-inference "${full[@]}"
! contains sandbox-linux-bubblewrap "${full[@]}"
contains local-inference "${sandbox[@]}"
contains sandbox-linux-bubblewrap "${sandbox[@]}"
! contains local-inference "${full_bin_managed[@]}"
! contains local-inference "${sandbox_bin_managed[@]}"
! contains sandbox-linux-bubblewrap "${sandbox_bin_managed[@]}"

# Activation is a signed RyeOS service contract. Installed bundle sources do
# not carry workload-specific operator assemblers or a packaging escape hatch
# that would revive them.
if find "$ROOT/bundles" -mindepth 2 -maxdepth 2 -name assemble.py -print -quit \
    | grep -q .; then
  printf '%s\n' "bundle-local assembler is forbidden" >&2
  exit 1
fi
! grep -Fq 'assemble.py' "$ROOT/scripts/pkg/install-local-direct.sh"
! grep -Fq 'assemble.py' "$ROOT/deploy/aur/ryeos/PKGBUILD"

printf '%s\n' "bundle set contract ok"
