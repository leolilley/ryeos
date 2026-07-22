#!/usr/bin/env bash

# Hermetic regression cases for publisher trust at deployment boundaries.
# This script sources policy helpers only; it does not build, install, or start
# RyeOS and does not require root.

set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# shellcheck source=deploy/entrypoint.sh
source "$root/deploy/entrypoint.sh"
# shellcheck source=scripts/pkg/install-local-direct.sh
source "$root/scripts/pkg/install-local-direct.sh"

expect_rejected() {
    if "$@" >/dev/null 2>&1; then
        echo "expected publisher trust policy to reject: $*" >&2
        exit 1
    fi
}

source_dir="$tmp/source"
dev_fingerprint="741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea"
dev_public_key="sDKyQ9rFxIduNjGtXq6aTrLlAg39177NzCT1+YYqpRk="
root_trust="$source_dir/.ai/PUBLISHER_TRUST.toml"
bundle_trust="$source_dir/core/PUBLISHER_TRUST.toml"
mkdir -p "$(dirname "$root_trust")" "$(dirname "$bundle_trust")"
printf 'public_key = "ed25519:%s"\nfingerprint = "%s"\n' \
    "$dev_public_key" "$dev_fingerprint" >"$root_trust"
printf 'public_key = "ed25519:%s"\nfingerprint = "%s"\n' \
    "$dev_public_key" "$dev_fingerprint" >"$bundle_trust"

# Container startup must ignore baked publisher pointers by default.
unset RYEOS_TRUST_BAKED_PUBLISHERS
collect_baked_publisher_trust_args "$source_dir"
[[ ${#TRUST_ARGS[@]} -eq 0 ]]

# The named opt-in preserves locally built/custom-signed image workflows.
RYEOS_TRUST_BAKED_PUBLISHERS=1
collect_baked_publisher_trust_args "$source_dir"
[[ ${#TRUST_ARGS[@]} -eq 4 ]]
[[ "${TRUST_ARGS[0]}" == "--trust-file" ]]
[[ "${TRUST_ARGS[1]}" == "$root_trust" ]]
[[ "${TRUST_ARGS[2]}" == "--trust-file" ]]
[[ "${TRUST_ARGS[3]}" == "$bundle_trust" ]]

# Misspelled opt-ins and opted-in images without trust docs fail closed.
RYEOS_TRUST_BAKED_PUBLISHERS=yes
expect_rejected collect_baked_publisher_trust_args "$source_dir"
RYEOS_TRUST_BAKED_PUBLISHERS=1
expect_rejected collect_baked_publisher_trust_args "$tmp/empty-source"

# The local installer accepts the compiled official root without an override,
# but a source-supplied dev/custom publisher requires the named flag.
official_trust="$tmp/official/PUBLISHER_TRUST.toml"
official_fingerprint="$(bash "$root/scripts/release/official-publisher-fingerprint.sh")"
official_public_key="52ibSX/VklcQK5eGaC10ELQ18hsWgUQtO/tKzeYlNgM="
mkdir -p "$(dirname "$official_trust")"
printf 'public_key = "ed25519:%s"\nfingerprint = "%s"\n' \
    "$official_public_key" "$official_fingerprint" >"$official_trust"
validate_source_publisher_trust "$official_trust" 0 "$official_fingerprint"
expect_rejected validate_source_publisher_trust "$root_trust" 0 "$official_fingerprint"
validate_source_publisher_trust "$root_trust" 1 "$official_fingerprint"
expect_rejected validate_source_publisher_trust "$tmp/missing.toml" 1 "$official_fingerprint"

# Repeated documents from one selected publisher are all validated but produce
# one operator-facing trust decision instead of one line per bundle.
trust_output="$(
    _RYEOS_TERM_WIDTH=200 validate_selected_source_publisher_trust \
        1 "$official_fingerprint" "$root_trust" "$bundle_trust" 2>&1
)"
[[ "$(grep -o "$dev_fingerprint" <<<"$trust_output" | wc -l)" -eq 1 ]]
[[ "$trust_output" == *"2 selected documents"* ]]

# A custom key cannot bypass the early guard by merely claiming the official
# fingerprint in text; classification hashes the decoded key material.
forged_trust="$tmp/forged/PUBLISHER_TRUST.toml"
mkdir -p "$(dirname "$forged_trust")"
printf 'public_key = "ed25519:%s"\nfingerprint = "%s"\n' \
    "$dev_public_key" "$official_fingerprint" >"$forged_trust"
expect_rejected validate_source_publisher_trust "$forged_trust" 0 "$official_fingerprint"

# Only publisher docs in the selected source boundary become init arguments.
# A stale/unrelated document below the package share directory is not authority.
installed_share="$tmp/installed-share"
mkdir -p "$installed_share/.ai" "$installed_share/core" "$installed_share/residual"
cp "$root_trust" "$installed_share/.ai/PUBLISHER_TRUST.toml"
cp "$bundle_trust" "$installed_share/core/PUBLISHER_TRUST.toml"
cp "$forged_trust" "$installed_share/residual/PUBLISHER_TRUST.toml"
collect_selected_source_trust_args "$installed_share" core
[[ ${#SELECTED_SOURCE_TRUST_ARGS[@]} -eq 4 ]]
[[ "${SELECTED_SOURCE_TRUST_ARGS[0]}" == "--trust-file" ]]
[[ "${SELECTED_SOURCE_TRUST_ARGS[1]}" == "$installed_share/.ai/PUBLISHER_TRUST.toml" ]]
[[ "${SELECTED_SOURCE_TRUST_ARGS[2]}" == "--trust-file" ]]
[[ "${SELECTED_SOURCE_TRUST_ARGS[3]}" == "$installed_share/core/PUBLISHER_TRUST.toml" ]]
[[ " ${SELECTED_SOURCE_TRUST_ARGS[*]} " != *" $installed_share/residual/PUBLISHER_TRUST.toml "* ]]

echo "publisher trust policy cases passed"
