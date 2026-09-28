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
[[ "${bundle_set_ids[*]}" == "full central-host standard local-inference hosted-node hosted-workflow bundle-source release-authority" ]]
for set_name in "${bundle_set_ids[@]}"; do
  mapfile -t members < <(ryeos_bundle_set_names "$set_name")
  contains central-auth "${members[@]}"
done

[[ "${hosted_workflow[*]}" == "core central-auth standard hosted-node codex render-sandbox opencode" ]]
[[ "${local_inference[*]}" == "core central-auth standard local-inference" ]]
for forbidden in hosted-node codex render-sandbox opencode web browser ryeos-ui; do
  ! contains "$forbidden" "${local_inference[@]}"
done
[[ "${release_artifacts[*]}" == "core central-auth standard web browser ryeos-ui hosted-node codex render-sandbox opencode local-inference tv-tracker-authoring bundle-source bundle-release" ]]
[[ "${release_authority[*]}" == "core central-auth standard web browser ryeos-ui hosted-node codex render-sandbox opencode local-inference bundle-release" ]]
[[ "${release_authority_host_support[*]}" == "ryeos-bundle-publisher" ]]
contains bundle-release "${release_authority[@]}"
! contains bundle-source "${release_authority[@]}"
render_specs="$ROOT/bundles/render-sandbox/specs"
render_fixture="$ROOT/crates/host-adapters/render-sandbox-adapter/fixtures"
for profile in provider-spec.json snapshot-production-spec.json settings.schema.json; do
  cmp -s "$render_specs/$profile" "$render_fixture/$profile"
  profile_hash="$(sha256sum "$render_specs/$profile" | cut -d' ' -f1)"
  grep -Fq -- "$profile_hash" "$ROOT/bundles/render-sandbox/.ai/manifest.source.yaml"
done
for ordinary_set in full central-host standard local-inference hosted-node hosted-workflow bundle-source; do
  mapfile -t ordinary_members < <(ryeos_bundle_set_names "$ordinary_set")
  ! contains bundle-release "${ordinary_members[@]}"
  [[ -z "$(ryeos_bundle_set_host_support_bins "$ordinary_set")" ]]
done
[[ -z "$(ryeos_bundle_set_host_support_bins release-artifacts)" ]]
for set_name in "${bundle_set_ids[@]}"; do
  [[ "$(ryeos_bundle_set_node_init_profile "$set_name")" == "$set_name" ]]
done
! ryeos_bundle_set_node_init_profile release-artifacts
! ryeos_bundle_set_node_init_profile unknown

mapfile -t node_init_profiles < <(ryeos_node_init_profile_names)
[[ "${node_init_profiles[*]}" == "full central-host standard local-inference hosted-node hosted-workflow bundle-source contained-workflow development release-authority" ]]
[[ "$(ryeos_node_init_profile_bundle_set contained-workflow)" == "hosted-workflow" ]]
[[ "$(ryeos_node_init_profile_bundle_set development)" == "full" ]]
[[ "$(ryeos_node_init_profile_bundle_set release-authority)" == "release-authority" ]]
! ryeos_node_init_profile_bundle_set release-artifacts
! ryeos_node_init_profile_bundle_set unknown
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
  ! ryeos_validate_node_init_root "$invalid_init_root" >/dev/null 2>&1
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
  if [[ "$profile_name" == hosted-workflow ]]; then
    grep -Eq '^      trusted_process_group_sessions: true$' "$node_init_profile"
  else
    grep -Eq '^      trusted_process_group_sessions: false$' "$node_init_profile"
  fi
  profile_bundle_set="$(ryeos_node_init_profile_bundle_set "$profile_name")"
  expected_exact_bundles="$(ryeos_bundle_set_names "$profile_bundle_set" | sort)"
  actual_exact_bundles="$(
    sed -n '/^exact_bundles:/,/^policies:/p' "$node_init_profile" \
      | sed -nE 's/^  - ([A-Za-z0-9_-]+)$/\1/p' \
      | sort
  )"
  [[ "$actual_exact_bundles" == "$expected_exact_bundles" ]]
  if [[ "$profile_name" == release-authority ]]; then
    grep -Eq '^  - bundle-release$' "$node_init_profile"
    grep -Eq '^      bundle-release:$' "$node_init_profile"
    grep -Eq '^        - ryeos\.execute\.service\.bundle-release/submit$' "$node_init_profile"
  else
    ! grep -Eq '^  - bundle-release$' "$node_init_profile"
    ! grep -Fq 'ryeos.execute.service.bundle-release/' "$node_init_profile"
  fi
done

contains local-inference "${full[@]}"
! contains local-inference "${full_bin_managed[@]}"
mapfile -t local_inference_bin_managed < <(ryeos_bundle_set_bin_managed_names local-inference)
[[ "${local_inference_bin_managed[*]}" == "core standard" ]]

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
grep -Fq -- 'build --release -p ryeosd -p ryeos-cli -p ryeos-core-tools' \
  <<<"$daemon_scope_output"
! grep -Fq -- '-p ryeos-session-exec' <<<"$daemon_scope_output"
! grep -Fq -- '-p ryeos-structured-session' <<<"$daemon_scope_output"
grep -Fq -- \
  "$scope_tmp/target/release/ryeos-core-tools" \
  <<<"$daemon_scope_output"
test -f "$scope_tmp/repo/bundles/core/.ai/refs/sentinel"

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
        --bundle-set full \
        --crates "$selected_package" 2>&1
  )"
  static_scope_status=$?
  set -e
  [[ "$static_scope_status" -eq 2 ]]
  # Exactly one static Cargo request: no host duplicate or implicit companion
  # package build. Every binary belonging to that package uses its target path.
  [[ "$(sed -n '/^build /p' <<<"$static_scope_output")" \
    == "build --release --target x86_64-unknown-linux-gnu -p $selected_package" ]]
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
  == 'build --release --target x86_64-unknown-linux-gnu -p ryeos-session-exec -p ryeos-structured-session -p ryeos-external-candidate-connector' ]]
test -f "$scope_tmp/repo/bundles/core/.ai/refs/sentinel"

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
done <<<"$daemon_scope_output"
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

printf '%s\n' "bundle set contract ok"
