#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$ROOT/scripts/pkg/bundle-sets.sh"

mapfile -t full < <(ryeos_bundle_set_names full)
mapfile -t sandbox < <(ryeos_bundle_set_names full-sandbox)
mapfile -t hosted_workflow < <(ryeos_bundle_set_names hosted-workflow)
mapfile -t release_artifacts < <(ryeos_bundle_set_names release-artifacts)
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
[[ "${bundle_set_ids[*]}" == "full full-sandbox central-host standard hosted-node hosted-workflow" ]]
for set_name in "${bundle_set_ids[@]}"; do
  mapfile -t members < <(ryeos_bundle_set_names "$set_name")
  contains central-auth "${members[@]}"
done

[[ "${hosted_workflow[*]}" == "core central-auth standard hosted-node codex" ]]
[[ "${release_artifacts[*]}" == "core central-auth standard web browser ryeos-ui hosted-node codex local-inference tv-tracker-authoring" ]]
for set_name in "${bundle_set_ids[@]}"; do
  [[ "$(ryeos_bundle_set_node_init_profile "$set_name")" == "$set_name" ]]
done
! ryeos_bundle_set_node_init_profile release-artifacts
! ryeos_bundle_set_node_init_profile unknown

mapfile -t node_init_profiles < <(ryeos_node_init_profile_names)
[[ "${node_init_profiles[*]}" == "${bundle_set_ids[*]}" ]]
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

for set_name in "${bundle_set_ids[@]}"; do
  node_init_profile="$node_init_profile_dir/$set_name.yaml"
  ryeos_validate_node_init_profile "$set_name" "$node_init_profile"
  verify_dev_signed_profile "$node_init_profile"
  expected_exact_bundles="$(ryeos_bundle_set_names "$set_name" | sort)"
  actual_exact_bundles="$(
    sed -n '/^exact_bundles:/,/^policies:/p' "$node_init_profile" \
      | sed -nE 's/^  - ([A-Za-z0-9_-]+)$/\1/p' \
      | sort
  )"
  [[ "$actual_exact_bundles" == "$expected_exact_bundles" ]]
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

# Targeted population must build only the selected package class and fail
# before cleaning bundle state when retained artifacts are unavailable. Run a
# copied authoring script against a disposable skeleton so this regression test
# can never mutate the checkout's generated bundle trees.
scope_tmp="$(mktemp -d)"
trap 'rm -rf "$scope_tmp"' EXIT
mkdir -p \
  "$scope_tmp/repo/scripts/lib" \
  "$scope_tmp/repo/scripts/pkg" \
  "$scope_tmp/repo/bundles/core/.ai/refs" \
  "$scope_tmp/repo/bundles/.ai/node/init/profiles" \
  "$scope_tmp/target"
cp "$ROOT/scripts/populate-bundles.sh" "$scope_tmp/repo/scripts/populate-bundles.sh"
cp "$ROOT/scripts/lib/ryeos-terminal.sh" "$scope_tmp/repo/scripts/lib/ryeos-terminal.sh"
cp "$ROOT/scripts/pkg/bundle-sets.sh" "$scope_tmp/repo/scripts/pkg/bundle-sets.sh"
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
grep -Fq -- 'build --release -p ryeosd' <<<"$daemon_scope_output"
! grep -Fq -- '-p ryeos-session-exec' <<<"$daemon_scope_output"
! grep -Fq -- '-p ryeos-structured-session' <<<"$daemon_scope_output"
grep -Fq -- \
  "$scope_tmp/repo/bundles/core/.ai/bin/x86_64-unknown-linux-gnu/ryeos-core-tools" \
  <<<"$daemon_scope_output"
test -f "$scope_tmp/repo/bundles/core/.ai/refs/sentinel"

set +e
static_scope_output="$(
  RYEOS_TTY=never \
  CARGO=/bin/echo \
  CARGO_TARGET_DIR="$scope_tmp/target" \
    "$scope_tmp/repo/scripts/populate-bundles.sh" \
      --key "$scope_tmp/publisher.pem" \
      --owner test \
      --bundle-set full \
      --crates ryeos-session-exec 2>&1
)"
static_scope_status=$?
set -e
[[ "$static_scope_status" -eq 2 ]]
grep -Fq -- 'build --release --target x86_64-unknown-linux-gnu -p ryeos-session-exec' \
  <<<"$static_scope_output"
! grep -Fq -- '-p ryeos-structured-session' <<<"$static_scope_output"
grep -Fq -- "$scope_tmp/target/x86_64-unknown-linux-gnu/release/ryeos-session-exec" \
  <<<"$static_scope_output"
test -f "$scope_tmp/repo/bundles/core/.ai/refs/sentinel"

printf '%s\n' "bundle set contract ok"
