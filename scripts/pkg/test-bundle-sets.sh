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

for set_name in full full-sandbox central-host standard hosted-node hosted-workflow; do
  mapfile -t members < <(ryeos_bundle_set_names "$set_name")
  contains central-auth "${members[@]}"
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
  "$scope_tmp/target"
cp "$ROOT/scripts/populate-bundles.sh" "$scope_tmp/repo/scripts/populate-bundles.sh"
cp "$ROOT/scripts/lib/ryeos-terminal.sh" "$scope_tmp/repo/scripts/lib/ryeos-terminal.sh"
cp "$ROOT/scripts/pkg/bundle-sets.sh" "$scope_tmp/repo/scripts/pkg/bundle-sets.sh"
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
