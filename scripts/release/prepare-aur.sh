#!/usr/bin/env bash

# Prepare deterministic AUR package metadata from an already-downloaded release
# archive. This script does not fetch anything: the release job must provide the
# exact GitHub tag archive it intends AUR consumers to download.

set -euo pipefail
export LC_ALL=C

usage() {
    echo "usage: $0 --tag vX.Y.Z --archive PATH --output DIR --signer-fingerprint HEX --expected-sha256 HEX" >&2
    exit 2
}

tag=""
archive=""
output=""
signer_fingerprint=""
expected_sha256=""
while (($#)); do
    case "$1" in
        --tag) tag="${2:-}"; shift 2 ;;
        --archive) archive="${2:-}"; shift 2 ;;
        --output) output="${2:-}"; shift 2 ;;
        --signer-fingerprint) signer_fingerprint="${2:-}"; shift 2 ;;
        --expected-sha256) expected_sha256="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done

[[ -n "$tag" && -n "$archive" && -n "$output" && -n "$signer_fingerprint" && -n "$expected_sha256" ]] || usage
[[ -f "$archive" ]] || { echo "AUR release: archive not found: $archive" >&2; exit 2; }
[[ "$signer_fingerprint" =~ ^[0-9A-Fa-f]{40,64}$ ]] || {
    echo "AUR release: signer fingerprint must be 40-64 hexadecimal characters" >&2
    exit 2
}
signer_fingerprint="${signer_fingerprint^^}"

root="$(cd "$(dirname "$0")/../.." && pwd)"
version="$("$root"/scripts/release/resolve-version.sh push "$tag" "")"

[[ "$(git -C "$root" cat-file -t "refs/tags/$tag" 2>/dev/null)" == tag ]] || {
    echo "AUR release: $tag must be an annotated signed tag" >&2
    exit 2
}
tag_commit="$(git -C "$root" rev-parse "$tag^{}")"
head_commit="$(git -C "$root" rev-parse HEAD)"
[[ "$tag_commit" == "$head_commit" ]] || {
    echo "AUR release: $tag resolves to $tag_commit, but release checkout is $head_commit" >&2
    exit 2
}

verify_status="$(mktemp)"
trap 'rm -f "$verify_status"' EXIT
if ! git -C "$root" verify-tag --raw "$tag" > /dev/null 2>"$verify_status"; then
    echo "AUR release: tag signature verification failed for $tag" >&2
    exit 2
fi
actual_signer="$(awk '/^\[GNUPG:\] VALIDSIG / { print toupper($3); exit }' "$verify_status")"
[[ -n "$actual_signer" ]] || {
    echo "AUR release: verified tag did not report a GPG signing fingerprint" >&2
    exit 2
}
[[ "$actual_signer" == "$signer_fingerprint" ]] || {
    echo "AUR release: tag signer $actual_signer does not match required signer $signer_fingerprint" >&2
    exit 2
}

archive_sha256="$(sha256sum "$archive" | awk '{print $1}')"
[[ "$archive_sha256" =~ ^[0-9a-f]{64}$ ]] || {
    echo "AUR release: failed to compute archive SHA-256" >&2
    exit 2
}
expected_sha256="${expected_sha256,,}"
[[ "$expected_sha256" =~ ^[0-9a-f]{64}$ ]] || {
    echo "AUR release: expected SHA-256 is not 64 hexadecimal characters" >&2
    exit 2
}
[[ "$archive_sha256" == "$expected_sha256" ]] || {
    echo "AUR release: archive SHA-256 mismatch" >&2
    exit 2
}

if [[ -d "$output" && -n "$(find "$output" -mindepth 1 -print -quit)" ]]; then
    echo "AUR release: output directory must be empty: $output" >&2
    exit 2
fi
mkdir -p "$output"
for package in ryeos ryeos-mcp; do
    package_output="$output/$package"
    mkdir -p "$package_output"
    sed \
        -e "s/^pkgver=RELEASE_VERSION$/pkgver=$version/" \
        -e "s/^sha256sums=('RELEASE_ARCHIVE_SHA256')$/sha256sums=('$archive_sha256')/" \
        "$root/deploy/aur/$package/PKGBUILD" > "$package_output/PKGBUILD"
    if grep -Eq 'RELEASE_(VERSION|ARCHIVE_SHA256)|SKIP' "$package_output/PKGBUILD"; then
        echo "AUR release: unresolved or unsafe checksum placeholder in $package" >&2
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
    shellcheck "$root/scripts/release/prepare-aur.sh"
fi

printf 'prepared AUR metadata for %s (%s)\n' "$tag" "$archive_sha256"
