#!/usr/bin/env bash
# Single source of truth for which bundles belong to each bundle set.
#
# Sourced by scripts/populate-bundles.sh (publish + bin staging),
# scripts/release/package-bundle-artifact.sh (official release archive), and
# scripts/pkg/install-local-direct.sh (ship + init). The AUR package consumes
# the resulting `full` release artifact, so authoring, release, and local
# installation cannot silently drift on bundle membership.
#
# `ryeos_bundle_set_names <set>` echoes the ordered bundle names for a set, one
# per line. `ryeos_bundle_set_node_init_profile <set>` echoes the one explicit,
# same-named source-root init profile required by that distribution. Every
# known set has a profile; an unknown set fails. Selection is publisher-authored
# bootstrap data supplied to `ryeos init`, never inferred from bundle presence.
#
# `central-auth` is a member of every set: it ships in the source tree and is
# discovered/parsed at init, so its manifest must stay current — but it owns no
# compiled binaries, so populate excludes it from bin staging (see
# populate-bundles.sh).

ryeos_bundle_set_ids() {
  printf '%s\n' full full-sandbox central-host standard hosted-node hosted-workflow
}

ryeos_bundle_set_names() {
  case "$1" in
    full)            printf '%s\n' core central-auth standard web browser ryeos-ui hosted-node codex local-inference ;;
    full-sandbox)    printf '%s\n' core central-auth standard web browser ryeos-ui hosted-node codex local-inference sandbox-linux-bubblewrap ;;
    central-host)    printf '%s\n' core central-auth standard web tv-tracker-authoring ;;
    standard)        printf '%s\n' core central-auth standard ;;
    hosted-node)     printf '%s\n' core central-auth hosted-node ;;
    hosted-workflow) printf '%s\n' core central-auth standard hosted-node codex ;;
    *) return 1 ;;
  esac
}

# Exact publisher-authored node init-profile inventory carried by the shared
# source-root `.ai`. Keep this closed rather than discovering arbitrary YAML:
# anything named here becomes selectable authority after publisher signing.
ryeos_node_init_profile_names() {
  ryeos_bundle_set_ids
}

ryeos_node_init_profile_file_names() {
  local name
  while IFS= read -r name; do
    printf '%s.yaml\n' "$name"
  done < <(ryeos_node_init_profile_names)
}

ryeos_node_init_profile_policy_names() {
  local profile_file="$1"
  sed -n '/^policies:/,$p' "$profile_file" \
    | sed -nE 's/^  ([^[:space:]][^:]*):.*$/\1/p'
}

# The source-root init namespace is a closed authority catalog. Profiles are
# the sole bootstrap input; legacy parallel seeds or arbitrary siblings must
# never be copied into an install or release artifact.
ryeos_validate_node_init_root() {
  local init_dir="$1"
  local actual

  [[ -d "$init_dir" && ! -L "$init_dir" ]] || {
    printf 'missing or unsafe node init root: %s\n' "$init_dir" >&2
    return 1
  }
  actual="$(find "$init_dir" -mindepth 1 -maxdepth 1 -printf '%y %f\n' | sort)"
  [[ "$actual" == "d profiles" ]] || {
    printf 'node init root contains unsupported authority inputs: %s\n' \
      "$init_dir" >&2
    return 1
  }
}

# Validate the packaging-level structure of one publisher-authored init profile.
# Rust's node-policy registry remains authoritative for the evolving section
# inventory and its cardinalities. This boundary deliberately does not
# duplicate those names: it proves that the profile cannot drift from its
# exact bundle set and that it carries a nonempty, canonical policy mapping
# before population signs it or an installer replaces live state.
ryeos_validate_node_init_profile() {
  local set_name="$1"
  local profile_file="$2"
  local expected_bundles actual_bundles actual_policies policy_name duplicates

  [[ -f "$profile_file" && ! -L "$profile_file" ]] || {
    printf 'missing or unsafe node init profile for %s: %s\n' \
      "$set_name" "$profile_file" >&2
    return 1
  }
  [[ "$(grep -Ec '^schema: 1$' "$profile_file")" -eq 1 \
      && "$(grep -Ec '^exact_bundles:$' "$profile_file")" -eq 1 \
      && "$(grep -Ec '^policies:$' "$profile_file")" -eq 1 ]] || {
    printf 'node init profile has an invalid top-level schema for %s: %s\n' \
      "$set_name" "$profile_file" >&2
    return 1
  }

  expected_bundles="$(ryeos_bundle_set_names "$set_name" | sort)" || return 1
  actual_bundles="$(
    sed -n '/^exact_bundles:/,/^policies:/p' "$profile_file" \
      | sed -nE 's/^  - ([A-Za-z0-9_-]+)$/\1/p'
  )"
  [[ "$actual_bundles" == "$expected_bundles" ]] || {
    printf 'node init profile exact_bundles mismatch for %s\n' "$set_name" >&2
    return 1
  }

  actual_policies="$(ryeos_node_init_profile_policy_names "$profile_file")"
  [[ -n "$actual_policies" ]] || {
    printf 'node init profile has an empty policy inventory for %s\n' "$set_name" >&2
    return 1
  }
  while IFS= read -r policy_name; do
    [[ ${#policy_name} -le 128 \
        && "$policy_name" =~ ^[a-z]([a-z0-9_-]*[a-z0-9])?$ \
        && "$policy_name" != *"__"* \
        && "$policy_name" != *"--"* ]] || {
      printf 'node init profile has a noncanonical policy name for %s: %s\n' \
        "$set_name" "$policy_name" >&2
      return 1
    }
  done <<< "$actual_policies"
  duplicates="$(printf '%s\n' "$actual_policies" | sort | uniq -d)"
  [[ -z "$duplicates" ]] || {
    printf 'node init profile has duplicate policy names for %s: %s\n' \
      "$set_name" "$duplicates" >&2
    return 1
  }
}

ryeos_bundle_set_node_init_profile() {
  case "$1" in
    full|full-sandbox|central-host|standard|hosted-node|hosted-workflow)
      printf '%s\n' "$1"
      ;;
    *) return 1 ;;
  esac
}

# Bundles in a set that own compiled binaries populate must stage/clean —
# every set member except `central-auth` (Python tool-support source, committed).
ryeos_bundle_set_bin_managed_names() {
  local name
  ryeos_bundle_set_names "$1" | while IFS= read -r name; do
    # central-auth (Python tool support) and tv-tracker-authoring (reuses
    # bin:core/ryeos-core-tools) own no compiled
    # binaries; local-inference and sandbox own separately built payloads.
    [[ "$name" == "central-auth" || "$name" == "tv-tracker-authoring" || "$name" == "local-inference" || "$name" == "sandbox-linux-bubblewrap" ]] && continue
    printf '%s\n' "$name"
  done
}
