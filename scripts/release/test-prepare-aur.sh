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

PATH="$tmp/bin:$PATH" "$root/scripts/release/prepare-aur.sh" \
    --tag v1.2.3 \
    --archive "$tmp/v1.2.3.tar.gz" \
    --output "$tmp/out" \
    --signer-fingerprint "$fingerprint" \
    --expected-sha256 "$expected"

for package in ryeos ryeos-mcp; do
    grep -qx 'pkgver=1.2.3' "$tmp/out/$package/PKGBUILD"
    grep -qx "sha256sums=('$expected')" "$tmp/out/$package/PKGBUILD"
    ! grep -Eq 'SKIP|RELEASE_(VERSION|ARCHIVE_SHA256)' "$tmp/out/$package/PKGBUILD"
done

if PATH="$tmp/bin:$PATH" "$root/scripts/release/prepare-aur.sh" \
    --tag v1.2.3 --archive "$tmp/v1.2.3.tar.gz" --output "$tmp/bad" \
    --signer-fingerprint FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF >/dev/null 2>&1; then
    echo "expected signer mismatch to fail" >&2
    exit 1
fi
