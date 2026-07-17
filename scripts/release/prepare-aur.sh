#!/usr/bin/env bash

# Prepare deterministic AUR package metadata from already-downloaded release
# inputs. This script does not fetch anything: the release job must provide the
# exact tag source archive and official signed bundle artifact AUR consumers
# will download.

set -euo pipefail
export LC_ALL=C
root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$root/scripts/lib/ryeos-terminal.sh"
ryeos_term_init

usage() {
    ryeos_term_fail "usage: $0 --tag vX.Y.Z --archive PATH --bundle-archive PATH --output DIR --signer-fingerprint HEX --expected-sha256 HEX --expected-bundle-sha256 HEX"
    exit 2
}

tag=""
archive=""
bundle_archive=""
output=""
signer_fingerprint=""
expected_sha256=""
expected_bundle_sha256=""
while (($#)); do
    case "$1" in
        --tag) tag="${2:-}"; shift 2 ;;
        --archive) archive="${2:-}"; shift 2 ;;
        --bundle-archive) bundle_archive="${2:-}"; shift 2 ;;
        --output) output="${2:-}"; shift 2 ;;
        --signer-fingerprint) signer_fingerprint="${2:-}"; shift 2 ;;
        --expected-sha256) expected_sha256="${2:-}"; shift 2 ;;
        --expected-bundle-sha256) expected_bundle_sha256="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done

[[ -n "$tag" && -n "$archive" && -n "$bundle_archive" && -n "$output" && -n "$signer_fingerprint" && -n "$expected_sha256" && -n "$expected_bundle_sha256" ]] || usage
[[ -f "$archive" ]] || { ryeos_term_fail "archive not found: $archive"; exit 2; }
[[ -f "$bundle_archive" ]] || { ryeos_term_fail "bundle archive not found: $bundle_archive"; exit 2; }
[[ "$signer_fingerprint" =~ ^[0-9A-Fa-f]{40,64}$ ]] || {
    ryeos_term_fail "signer fingerprint must be 40-64 hexadecimal characters"
    exit 2
}
signer_fingerprint="${signer_fingerprint^^}"

version="$("$root"/scripts/release/resolve-version.sh push "$tag" "")"
expected_bundle_name="ryeos-bundles-${version}-x86_64.tar.gz"
ryeos_term_begin PUBLISH "preparing AUR metadata"
[[ "$(basename "$bundle_archive")" == "$expected_bundle_name" ]] || {
    ryeos_term_fail "bundle archive must be named $expected_bundle_name"
    exit 2
}

[[ "$(git -C "$root" cat-file -t "refs/tags/$tag" 2>/dev/null)" == tag ]] || {
    ryeos_term_fail "$tag must be an annotated signed tag"
    exit 2
}
tag_commit="$(git -C "$root" rev-parse "$tag^{}")"
head_commit="$(git -C "$root" rev-parse HEAD)"
[[ "$tag_commit" == "$head_commit" ]] || {
    ryeos_term_fail "$tag resolves to $tag_commit, but release checkout is $head_commit"
    exit 2
}

verify_status="$(mktemp)"
archive_entries=""
archive_listing=""
cleanup_aur() {
    local status="$1"
    ryeos_term_handle_exit "$status"
    [[ -z "$verify_status" ]] || rm -f "$verify_status"
    [[ -z "$archive_entries" ]] || rm -f "$archive_entries"
    [[ -z "$archive_listing" ]] || rm -f "$archive_listing"
    return "$status"
}
trap 'cleanup_aur "$?"' EXIT
if ! git -C "$root" verify-tag --raw "$tag" > /dev/null 2>"$verify_status"; then
    ryeos_term_fail "tag signature verification failed for $tag"
    exit 2
fi
actual_signer="$(awk '/^\[GNUPG:\] VALIDSIG / { print toupper($3); exit }' "$verify_status")"
[[ -n "$actual_signer" ]] || {
    ryeos_term_fail "verified tag did not report a GPG signing fingerprint"
    exit 2
}
[[ "$actual_signer" == "$signer_fingerprint" ]] || {
    ryeos_term_fail "tag signer $actual_signer does not match required signer $signer_fingerprint"
    exit 2
}

archive_sha256="$(sha256sum "$archive" | awk '{print $1}')"
[[ "$archive_sha256" =~ ^[0-9a-f]{64}$ ]] || {
    ryeos_term_fail "failed to compute archive SHA-256"
    exit 2
}
expected_sha256="${expected_sha256,,}"
[[ "$expected_sha256" =~ ^[0-9a-f]{64}$ ]] || {
    ryeos_term_fail "expected SHA-256 is not 64 hexadecimal characters"
    exit 2
}
[[ "$archive_sha256" == "$expected_sha256" ]] || {
    ryeos_term_fail "source archive SHA-256 mismatch"
    exit 2
}

bundle_archive_sha256="$(sha256sum "$bundle_archive" | awk '{print $1}')"
[[ "$bundle_archive_sha256" =~ ^[0-9a-f]{64}$ ]] || {
    ryeos_term_fail "failed to compute bundle archive SHA-256"
    exit 2
}
expected_bundle_sha256="${expected_bundle_sha256,,}"
[[ "$expected_bundle_sha256" =~ ^[0-9a-f]{64}$ ]] || {
    ryeos_term_fail "expected bundle SHA-256 is not 64 hexadecimal characters"
    exit 2
}
[[ "$bundle_archive_sha256" == "$expected_bundle_sha256" ]] || {
    ryeos_term_fail "bundle archive SHA-256 mismatch"
    exit 2
}

# Check the release artifact schema before rendering metadata. The checksum is
# the immutable package-manager pin; these structural checks catch an operator
# selecting the wrong release asset despite giving it a matching digest.
bundle_root="ryeos-bundles-${version}-x86_64"
archive_entries="$(mktemp)"
archive_listing="$(mktemp)"
tar --absolute-names -tzf "$bundle_archive" > "$archive_entries"
tar --absolute-names -tvzf "$bundle_archive" > "$archive_listing"
if awk -v root="$bundle_root" '
    $0 != root && $0 != root "/" && index($0, root "/") != 1 { bad = 1 }
    /(^|\/)\.\.($|\/)/ || /^\// { bad = 1 }
    END { exit !bad }
' "$archive_entries"; then
    ryeos_term_fail "bundle archive contains a path outside $bundle_root"
    exit 2
fi
if awk 'substr($1, 1, 1) != "-" && substr($1, 1, 1) != "d" { bad = 1 }
        END { exit !bad }' "$archive_listing"; then
    ryeos_term_fail "bundle archive contains a link or special file"
    exit 2
fi
grep -qx "$bundle_root/.ai/PUBLISHER_TRUST.toml" "$archive_entries" || {
    ryeos_term_fail "bundle archive is missing source-root publisher metadata"
    exit 2
}

# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"
while IFS= read -r bundle; do
    grep -qx "$bundle_root/$bundle/PUBLISHER_TRUST.toml" "$archive_entries" || {
        ryeos_term_fail "bundle archive is missing $bundle publisher metadata"
        exit 2
    }
    grep -qx "$bundle_root/$bundle/.ai/" "$archive_entries" || {
        ryeos_term_fail "bundle archive is missing $bundle/.ai/"
        exit 2
    }
done < <(ryeos_bundle_set_names full)

official_fp="$("$root/scripts/release/official-publisher-fingerprint.sh")"
root_trust_doc="$(tar -xOzf "$bundle_archive" "$bundle_root/.ai/PUBLISHER_TRUST.toml")"
artifact_fp="$(printf '%s\n' "$root_trust_doc" | sed -n 's/^[[:space:]]*fingerprint[[:space:]]*=[[:space:]]*"\([0-9A-Fa-f]\{64\}\)"[[:space:]]*$/\1/p')"
artifact_owner="$(printf '%s\n' "$root_trust_doc" | sed -n 's/^[[:space:]]*owner[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p')"
[[ "${artifact_fp,,}" == "$official_fp" && "$artifact_owner" == "ryeos-official" ]] || {
    ryeos_term_fail "bundle artifact does not identify the compiled-in official publisher"
    exit 2
}

if [[ -d "$output" && -n "$(find "$output" -mindepth 1 -print -quit)" ]]; then
    ryeos_term_fail "output directory must be empty: $output"
    exit 2
fi
mkdir -p "$output"
for package in ryeos ryeos-mcp; do
    package_output="$output/$package"
    mkdir -p "$package_output"
    sed \
        -e "s/^pkgver=RELEASE_VERSION$/pkgver=$version/" \
        -e "s/RELEASE_ARCHIVE_SHA256/$archive_sha256/g" \
        -e "s/RELEASE_BUNDLE_ARCHIVE_SHA256/$bundle_archive_sha256/g" \
        "$root/deploy/aur/$package/PKGBUILD" > "$package_output/PKGBUILD"
    if grep -Eq 'RELEASE_(VERSION|ARCHIVE_SHA256|BUNDLE_ARCHIVE_SHA256)|SKIP' "$package_output/PKGBUILD"; then
        ryeos_term_fail "unresolved or unsafe checksum placeholder in $package"
        exit 2
    fi
    if [[ -f "$root/deploy/aur/$package/$package.install" ]]; then
        cp "$root/deploy/aur/$package/$package.install" "$package_output/$package.install"
    fi
    bash -n "$package_output/PKGBUILD"
    if command -v makepkg >/dev/null 2>&1; then
        (cd "$package_output" && makepkg --printsrcinfo > .SRCINFO)
    fi
    if command -v namcap >/dev/null 2>&1; then
        namcap "$package_output/PKGBUILD"
    fi
done

if command -v shellcheck >/dev/null 2>&1; then
    (cd "$root" && shellcheck -x scripts/release/prepare-aur.sh scripts/pkg/bundle-sets.sh)
fi

ryeos_term_end success "PUBLISH COMPLETE" "$tag"
ryeos_term_section "release"
ryeos_term_row "source" "$archive_sha256"
ryeos_term_row "bundles" "$bundle_archive_sha256"
