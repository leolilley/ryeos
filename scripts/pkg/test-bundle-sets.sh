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

contains local-inference "${full[@]}"
! contains sandbox-linux-bubblewrap "${full[@]}"
contains local-inference "${sandbox[@]}"
contains sandbox-linux-bubblewrap "${sandbox[@]}"
! contains local-inference "${full_bin_managed[@]}"
! contains local-inference "${sandbox_bin_managed[@]}"
! contains sandbox-linux-bubblewrap "${sandbox_bin_managed[@]}"

printf '%s\n' "bundle set contract ok"
