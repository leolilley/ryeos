#!/usr/bin/env bash
# Bake-log and post-build checks for the composition `*-pair` stages of
# Dockerfile.release. Sourced after bundle-sets.sh by a stage that has built
# its `features` array (entries such as `ryeosd/scheduling`, possibly
# comma-joined) from `ryeos_bundle_set_cargo_features`.
#
# `ryeos_report_cargo_features <stage>` prints the cargo feature arguments the
# stage is about to build with, so the bake log shows what was selected.
#
# `ryeos_assert_compiled_features <stage> <ryeosd-binary>` fails unless the
# built daemon's `build-info --features` lists exactly the `ryeosd/` features
# in `features`. A daemon missing a feature (for example the handlers for an
# installed bundle's services) otherwise only fails at startup.

ryeos_report_cargo_features() {
  echo "$1: cargo feature args: ${feature_args[*]:-<none>}"
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
