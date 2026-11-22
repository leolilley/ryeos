#!/usr/bin/env bash
# Bake-log and post-build checks for the composition `*-pair` stages of
# Dockerfile.release. Sourced after bundle-sets.sh by a stage that has built
# its `features` array (entries such as `ryeosd/scheduling`, possibly
# comma-joined) from `ryeos_bundle_set_cargo_features`.
#
# `ryeos_build_daemon_pair <stage>` builds ryeosd and then ryeos-cli into
# /build/target-cache, each in its own Cargo invocation with its own unqualified
# features, and prints each invocation so the bake log shows what was selected.
# One multi-package build with package-qualified features (`-p ryeosd -p
# ryeos-cli --features ryeosd/scheduling,...`) produced a ryeosd compiled with
# none of them; populate-bundles.sh builds featured packages the same way.
#
# `ryeos_assert_compiled_features <stage> <ryeosd-binary>` fails unless the
# built daemon's `build-info --features` lists exactly the `ryeosd/` features
# in `features`. A daemon missing a feature (for example the handlers for an
# installed bundle's services) otherwise only fails at startup.

ryeos_build_daemon_pair() {
  local stage="$1" package item feature
  local -a own parts args
  for package in ryeosd ryeos-cli; do
    own=()
    for item in "${features[@]}"; do
      IFS=, read -r -a parts <<<"$item"
      for feature in "${parts[@]}"; do
        if [[ "$feature" == "$package/"* ]]; then own+=("${feature#"$package/"}"); fi
      done
    done
    args=()
    if [ "${#own[@]}" -gt 0 ]; then args=(--features "$(IFS=,; printf '%s' "${own[*]}")"); fi
    echo "$stage: cargo build -p $package ${args[*]:-<no features>}"
    RYEOS_EXACT_BUILD_PROVENANCE=1 CARGO_TARGET_DIR=/build/target-cache \
      cargo build --release --locked --no-default-features -p "$package" "${args[@]}" || return 1
  done
}

ryeos_assert_compiled_features() {
  local stage="$1" binary="$2" expected compiled item
  expected="$(
    for item in "${features[@]}"; do
      tr ',' '\n' <<<"$item"
    done | sed -n 's|^ryeosd/||p' | LC_ALL=C sort -u
  )"
  compiled="$("$binary" build-info --features | LC_ALL=C sort -u)" || {
    echo "$stage: $binary build-info --features failed" >&2
    return 1
  }
  if [ "$expected" != "$compiled" ]; then
    echo "$stage: ryeosd feature mismatch: the selector chose [$(tr '\n' ' ' <<<"$expected")] but the built daemon compiled [$(tr '\n' ' ' <<<"$compiled")]; the feature was lost between selection and the compiled binary" >&2
    return 1
  fi
  echo "$stage: ryeosd compiled features verified: [$(tr '\n' ' ' <<<"$compiled")]"
}
