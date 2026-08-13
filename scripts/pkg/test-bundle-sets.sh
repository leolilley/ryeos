#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$ROOT/scripts/pkg/bundle-sets.sh"

mapfile -t full < <(ryeos_bundle_set_names full)
mapfile -t local < <(ryeos_bundle_set_names full-local-inference)
mapfile -t local_bin_managed < <(ryeos_bundle_set_bin_managed_names full-local-inference)

contains() {
  local needle="$1"
  shift
  local value
  for value in "$@"; do
    [[ "$value" == "$needle" ]] && return 0
  done
  return 1
}

! contains local-inference "${full[@]}"
! contains sandbox-linux-bubblewrap "${full[@]}"
contains local-inference "${local[@]}"
contains sandbox-linux-bubblewrap "${local[@]}"
! contains local-inference "${local_bin_managed[@]}"
! contains sandbox-linux-bubblewrap "${local_bin_managed[@]}"

printf '%s\n' "bundle set contract ok"
