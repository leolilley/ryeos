#!/usr/bin/env bash

# Hermetic regression cases for prepare-aur.sh. Uses a fake git verifier and a
# local archive fixture; it performs no network access or package build.

set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"
fingerprint="0123456789ABCDEF0123456789ABCDEF01234567"

cat > "$tmp/bin/git" <<EOF
#!/usr/bin/env bash
case "\$*" in
  *"cat-file -t refs/tags/v1.2.3") echo tag ;;
  *"rev-parse v1.2.3^{}"|*"rev-parse HEAD") echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ;;
  *"verify-tag --raw v1.2.3") echo '[GNUPG:] VALIDSIG $fingerprint 2026-01-01 0 4 0 1 10 00 0123456789ABCDEF' >&2 ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$tmp/bin/git"
printf 'release archive fixture\n' > "$tmp/v1.2.3.tar.gz"
expected="$(sha256sum "$tmp/v1.2.3.tar.gz" | awk '{print $1}')"

official_fp="$("$root/scripts/release/official-publisher-fingerprint.sh")"
bundle_root="$tmp/ryeos-bundles-1.2.3-x86_64"
mkdir -p "$bundle_root/.ai"
cat > "$bundle_root/.ai/PUBLISHER_TRUST.toml" <<EOF
public_key = "ed25519:test-fixture"
fingerprint = "$official_fp"
owner = "ryeos-official"
EOF
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"
while IFS= read -r bundle; do
    mkdir -p "$bundle_root/$bundle/.ai"
    cp "$bundle_root/.ai/PUBLISHER_TRUST.toml" \
        "$bundle_root/$bundle/PUBLISHER_TRUST.toml"
done < <(ryeos_bundle_set_names full)
tar -C "$tmp" -czf "$tmp/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    ryeos-bundles-1.2.3-x86_64
expected_bundle="$(sha256sum "$tmp/ryeos-bundles-1.2.3-x86_64.tar.gz" | awk '{print $1}')"

PATH="$tmp/bin:$PATH" "$root/scripts/release/prepare-aur.sh" \
    --tag v1.2.3 \
    --archive "$tmp/v1.2.3.tar.gz" \
    --bundle-archive "$tmp/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --output "$tmp/out" \
    --signer-fingerprint "$fingerprint" \
    --expected-sha256 "$expected" \
    --expected-bundle-sha256 "$expected_bundle"

for package in ryeos ryeos-mcp; do
    grep -qx 'pkgver=1.2.3' "$tmp/out/$package/PKGBUILD"
    grep -Fq "'$expected'" "$tmp/out/$package/PKGBUILD"
    ! grep -Eq 'SKIP|RELEASE_(VERSION|ARCHIVE_SHA256|BUNDLE_ARCHIVE_SHA256)' "$tmp/out/$package/PKGBUILD"
done
grep -Fq 'ryeos-bundles-$pkgver-x86_64.tar.gz::https://github.com/leolilley/ryeos/releases/download/v${pkgver}/ryeos-bundles-${pkgver}-x86_64.tar.gz' \
    "$tmp/out/ryeos/PKGBUILD"
grep -Fq "'$expected_bundle'" "$tmp/out/ryeos/PKGBUILD"
! grep -Fq "$expected_bundle" "$tmp/out/ryeos-mcp/PKGBUILD"

if PATH="$tmp/bin:$PATH" "$root/scripts/release/prepare-aur.sh" \
    --tag v1.2.3 \
    --archive "$tmp/v1.2.3.tar.gz" \
    --bundle-archive "$tmp/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --output "$tmp/bad" \
    --signer-fingerprint FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF \
    --expected-sha256 "$expected" \
    --expected-bundle-sha256 "$expected_bundle" >/dev/null 2>&1; then
    echo "expected signer mismatch to fail" >&2
    exit 1
fi

if PATH="$tmp/bin:$PATH" "$root/scripts/release/prepare-aur.sh" \
    --tag v1.2.3 \
    --archive "$tmp/v1.2.3.tar.gz" \
    --bundle-archive "$tmp/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --output "$tmp/bad-bundle" \
    --signer-fingerprint "$fingerprint" \
    --expected-sha256 "$expected" \
    --expected-bundle-sha256 ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff \
    >/dev/null 2>&1; then
    echo "expected bundle checksum mismatch to fail" >&2
    exit 1
fi
