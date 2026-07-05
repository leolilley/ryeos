#!/usr/bin/env bash
# Single source of truth for which bundles belong to each bundle set.
#
# Sourced by scripts/populate-bundles.sh (publish + bin staging) and
# scripts/pkg/install-local-direct.sh (ship + init). deploy/aur/ryeos/PKGBUILD
# ships every bundle directory present under bundles/, so it needs no
# enumeration — this file keeps the two shell installers from drifting apart
# (the class of bug where one enumerates five bundles and the tree has seven).
#
# `ryeos_bundle_set_names <set>` echoes the ordered bundle names for a set, one
# per line. `central-auth` is a member of every set: it ships in the source tree
# and is discovered/parsed at init, so its manifest must stay current — but it
# owns no compiled binaries, so populate excludes it from bin staging (see
# populate-bundles.sh).

ryeos_bundle_set_names() {
  case "$1" in
    full)            printf '%s\n' core central-auth standard web browser studio hosted-node ;;
    central-host)    printf '%s\n' core central-auth standard web ;;
    standard)        printf '%s\n' core central-auth standard ;;
    hosted-node)     printf '%s\n' core central-auth hosted-node ;;
    hosted-workflow) printf '%s\n' core central-auth standard hosted-node ;;
    *) return 1 ;;
  esac
}

# Bundles in a set that own compiled binaries populate must stage/clean —
# every set member except `central-auth` (Python source bin, committed).
ryeos_bundle_set_bin_managed_names() {
  local name
  ryeos_bundle_set_names "$1" | while IFS= read -r name; do
    [[ "$name" == "central-auth" ]] && continue
    printf '%s\n' "$name"
  done
}
