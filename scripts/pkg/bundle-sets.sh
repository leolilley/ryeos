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
# per line. `ryeos_bundle_set_node_init_profile <set>` echoes that
# distribution's default source-root init profile. A separately selected
# profile may map to the same exact bundle set through
# `ryeos_node_init_profile_bundle_set`; policy selection must not invent a
# duplicate distribution. Selection is publisher-authored bootstrap data
# supplied to `ryeos init`, never inferred from bundle presence.
#
# `central-auth` is optional application authentication above the workflow
# layer (knowledge, directive, graph), not
# native node/caller authority. Only explicitly composed app distributions
# include it; baseline and compute/authoring distributions omit it. It owns no
# compiled payloads and is published only when the selected set includes it.

ryeos_bundle_set_ids() {
  printf '%s\n' full central-host standard standard-central-auth local-inference hosted-workflow bundle-source core core-scheduling core-workflow core-knowledge f5r-controller release-authority
}

ryeos_bundle_set_names() {
  case "$1" in
    full)            printf '%s\n' core central-auth scheduling knowledge directive graph ryeos-docs environment-qualification web browser ryeos-ui codex opencode ;;
    central-host)    printf '%s\n' core central-auth scheduling knowledge directive graph ryeos-docs web ;;
    standard)        printf '%s\n' core scheduling knowledge directive graph ryeos-docs ;;
    standard-central-auth) printf '%s\n' core scheduling knowledge directive graph ryeos-docs central-auth ;;
    local-inference) printf '%s\n' core scheduling knowledge directive graph ryeos-docs local-inference ;;
    hosted-workflow) printf '%s\n' core scheduling knowledge directive graph ryeos-docs codex opencode ;;
    core)            printf '%s\n' core ;;
    core-scheduling) printf '%s\n' core scheduling ;;
    # Core plus the graph kind and runtime only: no model providers, directive
    # or knowledge.
    core-workflow)   printf '%s\n' core graph ;;
    # Core plus the knowledge kind and runtime only.
    core-knowledge)  printf '%s\n' core knowledge ;;
    # Controller of remote Codex authoring leases. Graph and knowledge supply
    # the kinds codex and render-sandbox require; the controller runs no
    # schedules, so scheduling is absent.
    f5r-controller)  printf '%s\n' core graph knowledge codex render-sandbox ;;
    bundle-source)   printf '%s\n' core bundle-source ;;
    release-authority)
      printf '%s\n' core graph environment-qualification bundle-release
      ;;
    # Internal publication superset. This is not an installable bundle set and
    # deliberately has no node init profile: one release build publishes every
    # bundle needed by the native archive and release images, whose final
    # stages still select one exact deployable set above.
    release-artifacts)
      printf '%s\n' core central-auth scheduling knowledge directive graph ryeos-docs environment-qualification web browser ryeos-ui codex render-sandbox opencode local-inference bundle-source bundle-release
      ;;
    *) return 1 ;;
  esac
}

# Select native capabilities from the same explicit distribution
# membership used for publication and installation. Cargo defaults remain empty;
# focused node/CLI rebuilds must carry the selected distribution's features too.
# The selection is derived from the signed payload-ownership item, which
# declares each bundle's payload features and daemon-linked host features.
RYEOS_BUNDLE_SETS_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

ryeos_bundle_set_cargo_features() {
  local set_name="$1" package="$2"
  python3 -I "$RYEOS_BUNDLE_SETS_ROOT/scripts/pkg/bundle-features.py" \
    --set "$set_name" cargo-features "$package"
}

# Exact publisher-authored node init-profile inventory carried by the shared
# source-root `.ai`. Keep this closed rather than discovering arbitrary YAML:
# anything named here becomes selectable authority after publisher signing.
ryeos_node_init_profile_names() {
  printf '%s\n' full central-host standard standard-central-auth local-inference hosted-node hosted-workflow bundle-source core core-scheduling core-workflow core-knowledge f5r-controller contained-workflow development release-authority
}

# Exact installed bundle set required by one publisher-authored profile.
# Profiles select policy, not payload membership: more than one closed policy
# generation may intentionally operate the same distribution.
ryeos_node_init_profile_bundle_set() {
  case "$1" in
    full|central-host|standard|standard-central-auth|local-inference|hosted-workflow|bundle-source|core|core-scheduling|core-workflow|core-knowledge|f5r-controller) printf '%s\n' "$1" ;;
    # The hosted-node profile is the core set under hosted admission policy.
    hosted-node) printf '%s\n' core ;;
    contained-workflow) printf '%s\n' hosted-workflow ;;
    development) printf '%s\n' full ;;
    release-authority) printf '%s\n' release-authority ;;
    *) return 1 ;;
  esac
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
  local profile_name="$1"
  local profile_file="$2"
  local set_name expected_bundles actual_bundles actual_policies policy_name duplicates

  set_name="$(ryeos_node_init_profile_bundle_set "$profile_name")" || {
    printf 'unknown node init profile: %s\n' "$profile_name" >&2
    return 1
  }

  [[ -f "$profile_file" && ! -L "$profile_file" ]] || {
    printf 'missing or unsafe node init profile for %s: %s\n' \
      "$profile_name" "$profile_file" >&2
    return 1
  }
  [[ "$(grep -Ec '^schema: 1$' "$profile_file")" -eq 1 \
      && "$(grep -Ec '^exact_bundles:$' "$profile_file")" -eq 1 \
      && "$(grep -Ec '^policies:$' "$profile_file")" -eq 1 ]] || {
    printf 'node init profile has an invalid top-level schema for %s: %s\n' \
      "$profile_name" "$profile_file" >&2
    return 1
  }

  expected_bundles="$(ryeos_bundle_set_names "$set_name" | sort)" || return 1
  actual_bundles="$(
    sed -n '/^exact_bundles:/,/^policies:/p' "$profile_file" \
      | sed -nE 's/^  - ([A-Za-z0-9_-]+)$/\1/p'
  )"
  [[ "$actual_bundles" == "$expected_bundles" ]] || {
    printf 'node init profile exact_bundles mismatch for %s\n' "$profile_name" >&2
    return 1
  }

  actual_policies="$(ryeos_node_init_profile_policy_names "$profile_file")"
  [[ -n "$actual_policies" ]] || {
    printf 'node init profile has an empty policy inventory for %s\n' "$profile_name" >&2
    return 1
  }
  while IFS= read -r policy_name; do
    [[ ${#policy_name} -le 128 \
        && "$policy_name" =~ ^[a-z]([a-z0-9_-]*[a-z0-9])?$ \
        && "$policy_name" != *"__"* \
        && "$policy_name" != *"--"* ]] || {
      printf 'node init profile has a noncanonical policy name for %s: %s\n' \
        "$profile_name" "$policy_name" >&2
      return 1
    }
  done <<< "$actual_policies"
  duplicates="$(printf '%s\n' "$actual_policies" | sort | uniq -d)"
  [[ -z "$duplicates" ]] || {
    printf 'node init profile has duplicate policy names for %s: %s\n' \
      "$profile_name" "$duplicates" >&2
    return 1
  }
}

ryeos_bundle_set_node_init_profile() {
  case "$1" in
    full|central-host|standard|standard-central-auth|local-inference|hosted-workflow|bundle-source|core|core-scheduling|core-workflow|core-knowledge|f5r-controller|release-authority)
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
    # scheduling (service/command content), central-auth (Python tool
    # support) and ryeos-docs (knowledge) own no compiled binaries;
    # local-inference owns separately built payloads.
    [[ "$name" == "scheduling" || "$name" == "ryeos-docs" || "$name" == "central-auth" || "$name" == "local-inference" || "$name" == "bundle-source" || "$name" == "bundle-release" || "$name" == "environment-qualification" ]] && continue
    printf '%s\n' "$name"
  done
}

# Standalone service-support executables are host deployment artifacts, not
# signed bundle payloads and not part of the generic node image. The
# release-authority distribution carries the publisher executable solely as
# the exact local measurement copy used by authority measurement; the
# independently operated publisher process receives the same measured bytes
# through its own deployment lifecycle.
ryeos_bundle_set_host_support_bins() {
  case "$1" in
    release-authority) printf '%s\n' ryeos-bundle-publisher ;;
    full|central-host|standard|standard-central-auth|local-inference|hosted-workflow|bundle-source|core|core-scheduling|core-workflow|core-knowledge|f5r-controller|release-artifacts)
      return 0
      ;;
    *) return 1 ;;
  esac
}
