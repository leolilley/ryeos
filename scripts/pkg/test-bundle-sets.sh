#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$ROOT/scripts/pkg/bundle-sets.sh"

mapfile -t full < <(ryeos_bundle_set_names full)
mapfile -t hosted_workflow < <(ryeos_bundle_set_names hosted-workflow)
mapfile -t local_inference < <(ryeos_bundle_set_names local-inference)
mapfile -t release_artifacts < <(ryeos_bundle_set_names release-artifacts)
mapfile -t release_authority < <(ryeos_bundle_set_names release-authority)
mapfile -t full_bin_managed < <(ryeos_bundle_set_bin_managed_names full)
mapfile -t release_authority_host_support < <(ryeos_bundle_set_host_support_bins release-authority)

contains() {
  local needle="$1"
  shift
  local value
  for value in "$@"; do
    [[ "$value" == "$needle" ]] && return 0
  done
  return 1
}

# A standalone `! command` masks errexit, even when the command unexpectedly
# succeeds. Keep negative assertions as ordinary failing commands instead.
assert_rejected() {
  if "$@"; then
    printf 'expected rejection: %s\n' "$*" >&2
    return 1
  fi
}

verify_dev_signed_profile() (
  set -euo pipefail
  local profile_file="$1" tmp key expected_fp header claimed_hash signature signer_fp actual_hash
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  key="$ROOT/.dev-keys/PUBLISHER_DEV.pem"
  [[ -s "$key" ]]
  expected_fp="$(
    openssl pkey -in "$key" -pubout -outform DER 2>/dev/null \
      | tail -c 32 \
      | sha256sum \
      | cut -d' ' -f1
  )"
  [[ "$(grep -c '^# ryeos:signed:' "$profile_file")" -eq 1 ]]
  header="$(head -n 1 "$profile_file")"
  [[ "$header" =~ ^#\ ryeos:signed:.+:([0-9a-f]{64}):([^:]+):([0-9a-f]{64})$ ]]
  claimed_hash="${BASH_REMATCH[1]}"
  signature="${BASH_REMATCH[2]}"
  signer_fp="${BASH_REMATCH[3]}"
  [[ "$signer_fp" == "$expected_fp" ]]
  sed '/^# ryeos:signed:/d' "$profile_file" > "$tmp/body"
  actual_hash="$(sha256sum "$tmp/body" | cut -d' ' -f1)"
  [[ "$actual_hash" == "$claimed_hash" ]]
  printf '%s' "$claimed_hash" > "$tmp/hash"
  printf '%s' "$signature" | base64 -d > "$tmp/signature"
  [[ "$(wc -c < "$tmp/signature")" -eq 64 ]]
  openssl pkey -in "$key" -pubout -out "$tmp/public.pem" 2>/dev/null
  openssl pkeyutl \
    -verify \
    -pubin \
    -inkey "$tmp/public.pem" \
    -rawin \
    -in "$tmp/hash" \
    -sigfile "$tmp/signature" >/dev/null 2>&1
)

mapfile -t bundle_set_ids < <(ryeos_bundle_set_ids)
[[ "${bundle_set_ids[*]}" == "full central-host standard standard-central-auth local-inference hosted-workflow bundle-source core core-scheduling f5r-controller release-authority" ]]
# Native node/caller authentication does not require application sessions.
for set_name in core core-scheduling f5r-controller standard local-inference hosted-workflow bundle-source; do
  mapfile -t members < <(ryeos_bundle_set_names "$set_name")
  assert_rejected contains central-auth "${members[@]}"
done
for set_name in full central-host standard-central-auth release-artifacts; do
  mapfile -t members < <(ryeos_bundle_set_names "$set_name")
  contains central-auth "${members[@]}"
  for workflow_member in knowledge directive graph ryeos-docs; do
    contains "$workflow_member" "${members[@]}"
  done
done
mapfile -t standard_baseline < <(ryeos_bundle_set_names standard)
[[ "${standard_baseline[*]}" == "core scheduling knowledge directive graph ryeos-docs" ]]
mapfile -t standard_auth < <(ryeos_bundle_set_names standard-central-auth)
[[ "${standard_auth[*]}" == "core scheduling knowledge directive graph ryeos-docs central-auth" ]]

# Host features derive in canonical sorted order from the bundles present.
for set_name in full central-host standard standard-central-auth local-inference hosted-workflow release-artifacts; do
  for package in ryeosd ryeos-cli ryeos-api; do
    [[ "$(ryeos_bundle_set_cargo_features "$set_name" "$package")" == "$package/model-provider-setup,$package/scheduling" ]]
  done
done
for package in ryeosd ryeos-cli ryeos-api; do
  [[ "$(ryeos_bundle_set_cargo_features core-scheduling "$package")" == "$package/scheduling" ]]
  # The controller carries no directive or scheduling, so no host features.
  [[ -z "$(ryeos_bundle_set_cargo_features f5r-controller "$package")" ]]
  [[ -z "$(ryeos_bundle_set_cargo_features release-authority "$package")" ]]
done
for set_name in bundle-source core; do
  for package in ryeosd ryeos-cli ryeos-api; do
    [[ -z "$(ryeos_bundle_set_cargo_features "$set_name" "$package")" ]]
  done
done
for set_name in full central-host standard standard-central-auth local-inference hosted-workflow release-artifacts; do
  [[ "$(ryeos_bundle_set_cargo_features "$set_name" ryeos-handler-bins)" == "ryeos-handler-bins/directive-handlers,ryeos-handler-bins/graph-handlers" ]]
done
for set_name in f5r-controller release-authority; do
  [[ "$(ryeos_bundle_set_cargo_features "$set_name" ryeos-handler-bins)" == "ryeos-handler-bins/graph-handlers" ]]
done
for set_name in core core-scheduling bundle-source; do
  [[ "$(ryeos_bundle_set_cargo_features "$set_name" ryeos-handler-bins)" == "ryeos-handler-bins/core-handlers,ryeos-handler-bins/extends-composer" ]]
done
# The parser package has no compositions: every set builds it with its defaults.
for set_name in full central-host standard standard-central-auth local-inference hosted-workflow bundle-source core core-scheduling f5r-controller release-authority release-artifacts; do
  [[ -z "$(ryeos_bundle_set_cargo_features "$set_name" ryeos-parser-bins)" ]]
done
mapfile -t core < <(ryeos_bundle_set_names core)
[[ "${core[*]}" == "core" ]]
mapfile -t core_bins < <(ryeos_bundle_set_bin_managed_names core)
[[ "${core_bins[*]}" == "core" ]]
for forbidden in central-auth scheduling knowledge directive graph ryeos-docs local-inference codex render-sandbox; do
  assert_rejected contains "$forbidden" "${core[@]}"
done
mapfile -t core_scheduling < <(ryeos_bundle_set_names core-scheduling)
[[ "${core_scheduling[*]}" == "core scheduling" ]]
mapfile -t core_scheduling_bins < <(ryeos_bundle_set_bin_managed_names core-scheduling)
[[ "${core_scheduling_bins[*]}" == "core" ]]
for forbidden in central-auth knowledge directive graph ryeos-docs local-inference codex render-sandbox; do
  assert_rejected contains "$forbidden" "${core_scheduling[@]}"
done
mapfile -t f5r_controller < <(ryeos_bundle_set_names f5r-controller)
[[ "${f5r_controller[*]}" == "core graph knowledge codex render-sandbox" ]]
mapfile -t f5r_controller_bins < <(ryeos_bundle_set_bin_managed_names f5r-controller)
[[ "${f5r_controller_bins[*]}" == "core graph knowledge codex render-sandbox" ]]
for forbidden in central-auth scheduling directive ryeos-docs opencode local-inference bundle-release; do
  assert_rejected contains "$forbidden" "${f5r_controller[@]}"
done
[[ -z "$(ryeos_bundle_set_cargo_features full ryeos-core-tools)" ]]
assert_rejected ryeos_bundle_set_cargo_features unknown ryeosd

[[ "${hosted_workflow[*]}" == "core scheduling knowledge directive graph ryeos-docs codex opencode" ]]
[[ "${local_inference[*]}" == "core scheduling knowledge directive graph ryeos-docs local-inference" ]]
for forbidden in codex render-sandbox opencode web browser ryeos-ui; do
  assert_rejected contains "$forbidden" "${local_inference[@]}"
done
[[ "${release_artifacts[*]}" == "core central-auth scheduling knowledge directive graph ryeos-docs environment-qualification web browser ryeos-ui codex render-sandbox opencode local-inference bundle-source bundle-release" ]]
[[ "${release_authority[*]}" == "core graph environment-qualification bundle-release" ]]
for forbidden in central-auth scheduling knowledge directive ryeos-docs web browser ryeos-ui codex opencode render-sandbox; do
  assert_rejected contains "$forbidden" "${release_authority[@]}"
done
# ryeos-docs is knowledge-only content and owns no compiled binaries.
mapfile -t standard_bins < <(ryeos_bundle_set_bin_managed_names standard)
[[ "${standard_bins[*]}" == "core knowledge directive graph" ]]
# Reference closure: bundles whose kinds depend on other bundles' kinds never
# appear without them.
for set_name in "${bundle_set_ids[@]}" release-artifacts; do
  mapfile -t members < <(ryeos_bundle_set_names "$set_name")
  if contains ryeos-ui "${members[@]}"; then
    for required in directive graph knowledge; do contains "$required" "${members[@]}"; done
  fi
  if contains directive "${members[@]}" || contains ryeos-docs "${members[@]}" \
      || contains opencode "${members[@]}" || contains codex "${members[@]}"; then
    contains knowledge "${members[@]}"
  fi
  if contains codex "${members[@]}" || contains render-sandbox "${members[@]}" \
      || contains ryeos-ui "${members[@]}" || contains bundle-release "${members[@]}" \
      || contains local-inference "${members[@]}"; then
    contains graph "${members[@]}"
  fi
done
[[ "${release_authority_host_support[*]}" == "ryeos-bundle-publisher" ]]
contains bundle-release "${release_authority[@]}"
for set_name in full release-authority release-artifacts; do
  mapfile -t members < <(ryeos_bundle_set_names "$set_name")
  contains environment-qualification "${members[@]}"
  mapfile -t managed < <(ryeos_bundle_set_bin_managed_names "$set_name")
  assert_rejected contains environment-qualification "${managed[@]}"
done
for set_name in central-host standard standard-central-auth local-inference hosted-workflow bundle-source core core-scheduling f5r-controller; do
  mapfile -t members < <(ryeos_bundle_set_names "$set_name")
  assert_rejected contains environment-qualification "${members[@]}"
done
assert_rejected contains bundle-source "${release_authority[@]}"
assert_rejected contains local-inference "${release_authority[@]}"
render_specs="$ROOT/bundles/render-sandbox/specs"
render_fixture="$ROOT/crates/host-adapters/render-sandbox-adapter/fixtures"
for profile in provider-spec.json snapshot-production-spec.json settings.schema.json; do
  cmp -s "$render_specs/$profile" "$render_fixture/$profile"
  profile_hash="$(sha256sum "$render_specs/$profile" | cut -d' ' -f1)"
  grep -Fq -- "$profile_hash" "$ROOT/bundles/render-sandbox/.ai/manifest.source.yaml"
done
# Both package layouts must retain bundle-owned runtime specifications, not
# just .ai metadata. Otherwise init can succeed but the daemon cannot open the
# exact provider spec named by the installed signed manifest.
grep -Fq 'sudo cp -a "$bundle_dir/specs" "$share_dir/$name/specs"' \
  "$ROOT/scripts/pkg/install-local-direct.sh"
grep -Fq 'cp -a "$bundle_dir/specs" "$pkgdir/usr/share/ryeos/$name/specs"' \
  "$ROOT/deploy/aur/ryeos/PKGBUILD"
for ordinary_set in full central-host standard standard-central-auth local-inference hosted-workflow bundle-source core core-scheduling f5r-controller; do
  mapfile -t ordinary_members < <(ryeos_bundle_set_names "$ordinary_set")
  assert_rejected contains bundle-release "${ordinary_members[@]}"
  [[ -z "$(ryeos_bundle_set_host_support_bins "$ordinary_set")" ]]
done
[[ -z "$(ryeos_bundle_set_host_support_bins release-artifacts)" ]]
for set_name in "${bundle_set_ids[@]}"; do
  [[ "$(ryeos_bundle_set_node_init_profile "$set_name")" == "$set_name" ]]
done
assert_rejected ryeos_bundle_set_node_init_profile release-artifacts
assert_rejected ryeos_bundle_set_node_init_profile unknown

mapfile -t node_init_profiles < <(ryeos_node_init_profile_names)
[[ "${node_init_profiles[*]}" == "full central-host standard standard-central-auth local-inference hosted-node hosted-workflow bundle-source core core-scheduling f5r-controller contained-workflow development release-authority" ]]
[[ "$(ryeos_node_init_profile_bundle_set contained-workflow)" == "hosted-workflow" ]]
[[ "$(ryeos_node_init_profile_bundle_set hosted-node)" == "core" ]]
assert_rejected ryeos_bundle_set_names hosted-node
[[ "$(ryeos_node_init_profile_bundle_set development)" == "full" ]]
[[ "$(ryeos_node_init_profile_bundle_set release-authority)" == "release-authority" ]]
assert_rejected ryeos_node_init_profile_bundle_set release-artifacts
assert_rejected ryeos_node_init_profile_bundle_set unknown
node_init_profile_dir="$ROOT/bundles/.ai/node/init/profiles"
[[ -d "$node_init_profile_dir" && ! -L "$node_init_profile_dir" ]]
ryeos_validate_node_init_root "$ROOT/bundles/.ai/node/init"
[[ -z "$(find "$node_init_profile_dir" -mindepth 1 -maxdepth 1 ! -type f -print -quit)" ]]
[[ -z "$(find "$node_init_profile_dir" -mindepth 1 -maxdepth 1 -type f -links +1 -print -quit)" ]]
expected_node_init_profiles="$(ryeos_node_init_profile_file_names | sort)"
actual_node_init_profiles="$(find "$node_init_profile_dir" -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | sort)"
if [[ "$actual_node_init_profiles" != "$expected_node_init_profiles" ]]; then
  printf 'node init-profile inventory mismatch\nexpected:\n%s\nactual:\n%s\n' \
    "$expected_node_init_profiles" "$actual_node_init_profiles" >&2
  exit 1
fi

(
  invalid_init_root="$(mktemp -d)"
  trap 'rm -rf "$invalid_init_root"' EXIT
  mkdir -p "$invalid_init_root/profiles" "$invalid_init_root/legacy-seed"
  assert_rejected ryeos_validate_node_init_root "$invalid_init_root" >/dev/null 2>&1
)

for profile_name in "${node_init_profiles[@]}"; do
  node_init_profile="$node_init_profile_dir/$profile_name.yaml"
  ryeos_validate_node_init_profile "$profile_name" "$node_init_profile"
  verify_dev_signed_profile "$node_init_profile"
  execution_section="$(
    awk '/^  execution:$/ { in_execution = 1; next }
         in_execution && /^  [a-z_]+:$/ { exit }
         in_execution { print }' "$node_init_profile"
  )"
  grep -Fxq '    schema: 4' <<<"$execution_section"
  grep -Eq '^    producer_resource_ceiling:($| null$)' <<<"$execution_section"
  # The isolation policy declares its host classes as data: the
  # composition profiles a strict local class and a hosted class, the hosted
  # workflow profiles only a hosted class, every other profile only its
  # local class. Initialization selects one, named by the deployment.
  grep -Fxq '      version: 10' "$node_init_profile"
  grep -Fxq '      host_classes:' "$node_init_profile"
  declared_host_classes="$(
    sed -n '/^      host_classes:$/,/^      [a-z_]*:/p' "$node_init_profile" \
      | sed -nE 's/^        ([a-z][a-z0-9_]*):$/\1/p' | tr '\n' ' '
  )"
  case "$profile_name" in
    core|core-scheduling|standard|f5r-controller)
      [[ "$declared_host_classes" == "hosted local " ]] ;;
    hosted-node|hosted-workflow)
      [[ "$declared_host_classes" == "hosted " ]] ;;
    *)
      [[ "$declared_host_classes" == "local " ]] ;;
  esac
  if [[ "$profile_name" == hosted-workflow || "$profile_name" == f5r-controller ]]; then
    assert_rejected grep -Eq '^          trusted_process_group_sessions: false$' "$node_init_profile"
    grep -Eq '^          trusted_process_group_sessions: true$' "$node_init_profile"
  else
    assert_rejected grep -Eq '^          trusted_process_group_sessions: true$' "$node_init_profile"
    grep -Eq '^          trusted_process_group_sessions: false$' "$node_init_profile"
  fi
  profile_bundle_set="$(ryeos_node_init_profile_bundle_set "$profile_name")"
  expected_exact_bundles="$(ryeos_bundle_set_names "$profile_bundle_set" | sort)"
  actual_exact_bundles="$(
    sed -n '/^exact_bundles:/,/^policies:/p' "$node_init_profile" \
      | sed -nE 's/^  - ([A-Za-z0-9_-]+)$/\1/p' \
      | sort
  )"
  [[ "$actual_exact_bundles" == "$expected_exact_bundles" ]]
  # Every active source bundle needs an explicit command-registration entry,
  # including bundles that own no commands. A missing empty entry otherwise
  # refuses fresh node initialization before any runtime admission check.
  actual_command_source_bundles="$(
    sed -n '/^    bundle_source_caps:/,/^  execution:/p' "$node_init_profile" \
      | sed -nE 's/^      ([A-Za-z0-9_-]+):.*/\1/p' \
      | sort
  )"
  [[ "$actual_command_source_bundles" == "$expected_exact_bundles" ]]
  if [[ "$profile_name" == release-authority ]]; then
    grep -Eq '^  - bundle-release$' "$node_init_profile"
    grep -Eq '^      bundle-release:$' "$node_init_profile"
    grep -Eq '^        - ryeos\.execute\.service\.bundle-release/submit$' "$node_init_profile"
  else
    assert_rejected grep -Eq '^  - bundle-release$' "$node_init_profile"
    assert_rejected grep -Fq 'ryeos.execute.service.bundle-release/' "$node_init_profile"
  fi
done

assert_rejected contains local-inference "${full[@]}"
assert_rejected contains local-inference "${full_bin_managed[@]}"
mapfile -t local_inference_bin_managed < <(ryeos_bundle_set_bin_managed_names local-inference)
[[ "${local_inference_bin_managed[*]}" == "core knowledge directive graph" ]]

# Activation is a signed RyeOS service contract. Installed bundle sources do
# not carry workload-specific operator assemblers or a packaging escape hatch
# that would revive them.
if find "$ROOT/bundles" -mindepth 2 -maxdepth 2 -name assemble.py -print -quit \
    | grep -q .; then
  printf '%s\n' "bundle-local assembler is forbidden" >&2
  exit 1
fi
assert_rejected grep -Fq 'assemble.py' "$ROOT/scripts/pkg/install-local-direct.sh"
assert_rejected grep -Fq 'assemble.py' "$ROOT/deploy/aur/ryeos/PKGBUILD"

# Targeted population must build only the selected package class and fail
# before cleaning bundle state when retained artifacts are unavailable. Run a
# copied authoring script against a disposable skeleton so this regression test
# can never mutate the checkout's generated bundle trees.
scope_tmp="$(mktemp -d)"
trap 'rm -rf "$scope_tmp"' EXIT
mkdir -p \
  "$scope_tmp/repo/scripts/lib" \
  "$scope_tmp/repo/scripts/pkg" \
  "$scope_tmp/repo/scripts/release" \
  "$scope_tmp/repo/bundles/bundle-release/.ai/config/bundle-release" \
  "$scope_tmp/repo/bundles/.ai" \
  "$scope_tmp/repo/bundles/core/.ai/refs" \
  "$scope_tmp/repo/bundles/.ai/node/init/profiles" \
  "$scope_tmp/target"
cp "$ROOT/scripts/populate-bundles.sh" "$scope_tmp/repo/scripts/populate-bundles.sh"
cp "$ROOT/scripts/lib/ryeos-terminal.sh" "$scope_tmp/repo/scripts/lib/ryeos-terminal.sh"
cp "$ROOT/scripts/pkg/bundle-sets.sh" "$scope_tmp/repo/scripts/pkg/bundle-sets.sh"
cp "$ROOT/scripts/pkg/bundle-features.py" "$scope_tmp/repo/scripts/pkg/bundle-features.py"
cp "$ROOT/scripts/release/bundle-payload-ownership.py" "$scope_tmp/repo/scripts/release/bundle-payload-ownership.py"
cp "$ROOT/bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml" "$scope_tmp/repo/bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml"
cp "$ROOT/bundles/.ai/PUBLISHER_TRUST.toml" "$scope_tmp/repo/bundles/.ai/PUBLISHER_TRUST.toml"
cp "$node_init_profile_dir"/*.yaml "$scope_tmp/repo/bundles/.ai/node/init/profiles/"
touch "$scope_tmp/repo/bundles/core/.ai/refs/sentinel"
openssl genpkey -algorithm ED25519 -out "$scope_tmp/publisher.pem" 2>/dev/null

set +e
daemon_scope_output="$(
  RYEOS_TTY=never \
  CARGO=/bin/echo \
  CARGO_TARGET_DIR="$scope_tmp/target" \
    "$scope_tmp/repo/scripts/populate-bundles.sh" \
      --key "$scope_tmp/publisher.pem" \
      --owner test \
      --bundle-set full \
      --crates ryeosd 2>&1
)"
daemon_scope_status=$?
set -e
[[ "$daemon_scope_status" -eq 2 ]]
grep -Fq -- 'build --release --locked -p ryeosd -p ryeos-cli -p ryeos-core-tools' \
  <<<"$daemon_scope_output"
grep -Fq -- '--features ryeosd/model-provider-setup,ryeosd/scheduling,ryeos-cli/model-provider-setup,ryeos-cli/scheduling' \
  <<<"$daemon_scope_output"
assert_rejected grep -Fq -- '-p ryeos-session-exec' <<<"$daemon_scope_output"
assert_rejected grep -Fq -- '-p ryeos-structured-session' <<<"$daemon_scope_output"
grep -Fq -- \
  "$scope_tmp/target/release/ryeos-core-tools" \
  <<<"$daemon_scope_output"
test -f "$scope_tmp/repo/bundles/core/.ai/refs/sentinel"

# A focused node rebuild must preserve the smaller distribution selection,
# even though the daemon's implicit CLI/core-tools companions are included.
set +e
reduced_daemon_output="$(
  RYEOS_TTY=never \
  CARGO=/bin/echo \
  CARGO_TARGET_DIR="$scope_tmp/target" \
    "$scope_tmp/repo/scripts/populate-bundles.sh" \
      --key "$scope_tmp/publisher.pem" \
      --owner test \
      --bundle-set core \
      --crates ryeosd 2>&1
)"
reduced_daemon_status=$?
set -e
[[ "$reduced_daemon_status" -eq 2 ]]
grep -Fq -- 'build --release --locked -p ryeosd -p ryeos-cli -p ryeos-core-tools' \
  <<<"$reduced_daemon_output"
assert_rejected grep -Fq -- 'scheduling' <<<"$reduced_daemon_output"
assert_rejected grep -Fq -- 'model-provider-setup' <<<"$reduced_daemon_output"
test -f "$scope_tmp/repo/bundles/core/.ai/refs/sentinel"

# The controller carries neither the directive provider setup nor the
# scheduler, so its rebuild selects no host features.
set +e
controller_daemon_output="$(
  RYEOS_TTY=never \
  CARGO=/bin/echo \
  CARGO_TARGET_DIR="$scope_tmp/target" \
    "$scope_tmp/repo/scripts/populate-bundles.sh" \
      --key "$scope_tmp/publisher.pem" \
      --owner test \
      --bundle-set f5r-controller \
      --crates ryeosd 2>&1
)"
controller_daemon_status=$?
set -e
[[ "$controller_daemon_status" -eq 2 ]]
assert_rejected grep -Fq -- 'scheduling' <<<"$controller_daemon_output"
assert_rejected grep -Fq -- 'model-provider-setup' <<<"$controller_daemon_output"

static_package_cases=(
  'ryeos-session-exec|ryeos-session-exec'
  'ryeos-structured-session|ryeos-worker-execution-launch-preparer ryeos-worker-execution-runtime ryeos-structured-session-bridge'
  'ryeos-external-candidate-connector|ryeos-external-candidate-connector'
  'ryeos-external-guest-occurrence-owner|ryeos-external-guest-occurrence-owner'
  'ryeos-external-guest-restoration-verifier|ryeos-external-guest-restoration-verifier'
  'ryeos-external-candidate-launcher|ryeos-external-candidate-launcher'
  'ryeos-external-candidate-supervisor|ryeos-external-candidate-supervisor'
  'ryeos-render-sandbox-lifecycle-adapter|ryeos-render-sandbox-lifecycle-adapter'
  'ryeos-external-guest-runtime-producer|ryeos-external-guest-runtime-producer'
  'ryeos-codex-external-configuration|ryeos-codex-external-configuration'
  'ryeos-codex-guest-runtime-producer|ryeos-codex-guest-runtime-producer'
  'ryeos-codex-runtime-verifier|codex-runtime-verifier'
)
for package_case in "${static_package_cases[@]}"; do
  selected_package="${package_case%%|*}"
  read -ra expected_outputs <<<"${package_case#*|}"
  set +e
  static_scope_output="$(
    RYEOS_TTY=never \
    CARGO=/bin/echo \
    CARGO_TARGET_DIR="$scope_tmp/target" \
      "$scope_tmp/repo/scripts/populate-bundles.sh" \
        --key "$scope_tmp/publisher.pem" \
        --owner test \
        --bundle-set release-artifacts \
        --crates "$selected_package" 2>&1
  )"
  static_scope_status=$?
  set -e
  [[ "$static_scope_status" -eq 2 ]]
  # Exactly one static Cargo request: no host duplicate or implicit companion
  # package build. Every binary belonging to that package uses its target path.
  [[ "$(sed -n '/^build /p' <<<"$static_scope_output")" \
    == "build --release --locked --target x86_64-unknown-linux-gnu -p $selected_package" ]]
  for binary in "${expected_outputs[@]}"; do
    grep -Fxq -- \
      "    - $scope_tmp/target/x86_64-unknown-linux-gnu/release/$binary" \
      <<<"$static_scope_output"
  done
  grep -Fxq -- \
    "    - $scope_tmp/repo/bundles/core/.ai/bin/x86_64-unknown-linux-gnu/ryeos-core-tools" \
    <<<"$static_scope_output"
  test -f "$scope_tmp/repo/bundles/core/.ai/refs/sentinel"
done

# Multiple selected static packages must share one Cargo invocation. This
# prevents repeated dependency-graph setup while retaining exact target paths.
set +e
combined_static_output="$(
  RYEOS_TTY=never \
  CARGO=/bin/echo \
  CARGO_TARGET_DIR="$scope_tmp/target" \
    "$scope_tmp/repo/scripts/populate-bundles.sh" \
      --key "$scope_tmp/publisher.pem" \
      --owner test \
      --bundle-set full \
      --crates 'ryeos-session-exec ryeos-structured-session ryeos-external-candidate-connector' 2>&1
)"
combined_static_status=$?
set -e
[[ "$combined_static_status" -eq 2 ]]
[[ "$(sed -n '/^build /p' <<<"$combined_static_output")" \
  == 'build --release --locked --target x86_64-unknown-linux-gnu -p ryeos-session-exec -p ryeos-structured-session -p ryeos-external-candidate-connector' ]]
test -f "$scope_tmp/repo/bundles/core/.ai/refs/sentinel"

# Stage retained payloads for the widest set, so every set checked below
# (including those carrying render-sandbox, which full no longer does) finds
# its artifacts.
set +e
artifacts_scope_output="$(
  RYEOS_TTY=never \
  CARGO=/bin/echo \
  CARGO_TARGET_DIR="$scope_tmp/target" \
    "$scope_tmp/repo/scripts/populate-bundles.sh" \
      --key "$scope_tmp/publisher.pem" \
      --owner test \
      --bundle-set release-artifacts \
      --crates ryeosd 2>&1
)"
set -e

# Exercise the real population control flow with disposable retained payloads.
# Cargo remains inert. The deletion fence aborts before source signing or
# publication if an invalid payload incorrectly passes static qualification.
while IFS= read -r line; do
  case "$line" in
    "    - $scope_tmp/repo/bundles/"*|"    - $scope_tmp/target/"*)
      artifact="${line#    - }"
      mkdir -p "$(dirname "$artifact")"
      cp /bin/true "$artifact"
      ;;
  esac
done <<<"$daemon_scope_output
$artifacts_scope_output"
mkdir -p "$scope_tmp/probes"
real_readelf="$(command -v readelf)"
real_rm="$(command -v rm)"
cat > "$scope_tmp/probes/readelf" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
artifact="${@: -1}"
printf '%s %s\n' "$1" "$artifact" >> "$TEST_ELF_OBSERVATIONS"
if [[ "$artifact" == */ryeos-structured-session-bridge ]]; then
  case "$TEST_BRIDGE_ELF" in
    interpreter) printf '%s\n' '  INTERP 0x0000000000000040'; exit 0 ;;
    dependency) printf '%s\n' '  (NEEDED) Shared library: [libc.so.6]'; exit 0 ;;
    invalid) exec "$TEST_REAL_READELF" "$@" ;;
    inspection-failure) printf '%s\n' 'fixture ELF inspection failed' >&2; exit 19 ;;
    *) exit 20 ;;
  esac
fi
# Other fixture payloads stand for successfully inspected static ELF objects.
printf '%s\n' 'ELF Header:' 'There is no dynamic section in this file.'
EOF
cat > "$scope_tmp/probes/rm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
for argument in "$@"; do
  case "$argument" in
    "$TEST_SCOPE_REPO"/bundles/*/.ai/bin)
      printf '%s\n' 'invalid payload reached destructive bundle preparation' >&2
      exit 87
      ;;
  esac
done
exec "$TEST_REAL_RM" "$@"
EOF
chmod 0755 "$scope_tmp/probes/readelf" "$scope_tmp/probes/rm"

static_failures=0
assert_bridge_rejected_before_publication() {
  local bundle_set="$1" elf_kind="$2" output status
  : > "$scope_tmp/elf-observations"
  set +e
  output="$(
    PATH="$scope_tmp/probes:$PATH" \
    TEST_BRIDGE_ELF="$elf_kind" \
    TEST_REAL_READELF="$real_readelf" \
    TEST_REAL_RM="$real_rm" \
    TEST_SCOPE_REPO="$scope_tmp/repo" \
    TEST_ELF_OBSERVATIONS="$scope_tmp/elf-observations" \
    RYEOS_TTY=never \
    CARGO=/bin/echo \
    CARGO_TARGET_DIR="$scope_tmp/target" \
      "$scope_tmp/repo/scripts/populate-bundles.sh" \
        --key "$scope_tmp/publisher.pem" \
        --owner test \
        --bundle-set "$bundle_set" \
        --crates ryeosd 2>&1
  )"
  status=$?
  set -e
  if [[ "$status" -ne 2 ]] \
      || ! grep -Fq '/ryeos-structured-session-bridge' "$scope_tmp/elf-observations" \
      || grep -Fq 'invalid payload reached destructive bundle preparation' <<<"$output" \
      || [[ ! -f "$scope_tmp/repo/bundles/core/.ai/refs/sentinel" ]]; then
    printf 'static payload gate failed: set=%s bridge=%s status=%s\n%s\n' \
      "$bundle_set" "$elf_kind" "$status" "$output" >&2
    static_failures=$((static_failures + 1))
  fi
}

# The bridge belongs to core in every publication set, not only hosted sets.
for set_name in "${bundle_set_ids[@]}" release-artifacts; do
  assert_bridge_rejected_before_publication "$set_name" interpreter
done
assert_bridge_rejected_before_publication hosted-workflow dependency
assert_bridge_rejected_before_publication hosted-workflow inspection-failure
# Let the real inspector reject an executable text file, rather than equating
# lack of INTERP/NEEDED output with proof that inspection succeeded.
printf '%s\n' 'not an ELF executable' \
  > "$scope_tmp/repo/bundles/core/.ai/bin/x86_64-unknown-linux-gnu/ryeos-structured-session-bridge"
assert_bridge_rejected_before_publication hosted-workflow invalid
if (( static_failures > 0 )); then
  printf 'static payload gate regressions: %s\n' "$static_failures" >&2
  exit 1
fi

python3 -I "$ROOT/scripts/pkg/test-bundle-features.py"
python3 -I "$ROOT/scripts/pkg/test-bundle-features-signature.py"

printf '%s\n' "bundle set contract ok"
