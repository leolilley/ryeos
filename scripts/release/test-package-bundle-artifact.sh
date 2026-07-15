#!/usr/bin/env bash

# Hermetic regression cases for package-bundle-artifact.sh. The fake `ryeos`
# checks that release verification never supplies a trust-file override.

set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

official_fp="$("$root/scripts/release/official-publisher-fingerprint.sh")"
source_dir="$tmp/source"
mkdir -p "$source_dir/.ai"

write_trust_doc() {
    local path="$1"
    mkdir -p "$(dirname "$path")"
    cat > "$path" <<EOF
public_key = "ed25519:test-fixture"
fingerprint = "$official_fp"
owner = "ryeos-official"
EOF
}

write_trust_doc "$source_dir/.ai/PUBLISHER_TRUST.toml"
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"
while IFS= read -r bundle; do
    mkdir -p "$source_dir/$bundle/.ai"
    printf 'name: %s\n' "$bundle" > "$source_dir/$bundle/.ai/manifest.yaml"
    write_trust_doc "$source_dir/$bundle/PUBLISHER_TRUST.toml"
done < <(ryeos_bundle_set_names full)

fake_ryeos="$tmp/ryeos"
cat > "$fake_ryeos" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ " $* " != *" --trust-file "* ]] || {
    echo "release verification must not accept a trust-file override" >&2
    exit 2
}
[[ "${1:-}" == init && "${2:-}" == --app-root && "${4:-}" == --source ]]
[[ -d "${5:-}/.ai" ]]
mkdir -p "${3:-}"
EOF
chmod +x "$fake_ryeos"

for run in one two; do
    mkdir -p "$tmp/$run"
    "$root/scripts/release/package-bundle-artifact.sh" \
        --version 1.2.3 \
        --source "$source_dir" \
        --output "$tmp/$run/ryeos-bundles-1.2.3-x86_64.tar.gz" \
        --source-date-epoch 1700000000 \
        --ryeos-bin "$fake_ryeos" >/dev/null
    (cd "$tmp/$run" && sha256sum -c ryeos-bundles-1.2.3-x86_64.tar.gz.sha256)
    "$root/scripts/release/verify-bundle-artifact.sh" \
        --version 1.2.3 \
        --archive "$tmp/$run/ryeos-bundles-1.2.3-x86_64.tar.gz" \
        --checksum "$tmp/$run/ryeos-bundles-1.2.3-x86_64.tar.gz.sha256" >/dev/null
done

cmp "$tmp/one/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    "$tmp/two/ryeos-bundles-1.2.3-x86_64.tar.gz"
tar -tzf "$tmp/one/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    | grep -qx 'ryeos-bundles-1.2.3-x86_64/core/.ai/manifest.yaml'

mkdir "$tmp/malformed"
malformed_checksum="$tmp/malformed/ryeos-bundles-1.2.3-x86_64.tar.gz.sha256"
cp "$tmp/one/ryeos-bundles-1.2.3-x86_64.tar.gz.sha256" "$malformed_checksum"
printf '%s\n' 'unexpected trailing checksum entry' >> "$malformed_checksum"
if "$root/scripts/release/verify-bundle-artifact.sh" \
    --version 1.2.3 \
    --archive "$tmp/one/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --checksum "$malformed_checksum" >/dev/null 2>&1; then
    echo "expected a non-canonical checksum file to be rejected" >&2
    exit 1
fi

retry_fixture="$tmp/retry-private-key"
mkdir -p "$retry_fixture/tree"
tar -xzf "$tmp/one/ryeos-bundles-1.2.3-x86_64.tar.gz" -C "$retry_fixture/tree"
printf '%s\n' '-----BEGIN PRIVATE KEY-----' > \
    "$retry_fixture/tree/ryeos-bundles-1.2.3-x86_64/core/leaked-material.txt"
retry_archive="$retry_fixture/ryeos-bundles-1.2.3-x86_64.tar.gz"
tar -C "$retry_fixture/tree" -czf "$retry_archive" ryeos-bundles-1.2.3-x86_64
(
    cd "$retry_fixture"
    sha256sum "$(basename "$retry_archive")" > "$(basename "$retry_archive").sha256"
)
if "$root/scripts/release/verify-bundle-artifact.sh" \
    --version 1.2.3 \
    --archive "$retry_archive" \
    --checksum "$retry_archive.sha256" >/dev/null 2>&1; then
    echo "expected retry verification to reject private key material" >&2
    exit 1
fi

printf '%s\n' '-----BEGIN PRIVATE KEY-----' > "$source_dir/core/leaked.pem"
if "$root/scripts/release/package-bundle-artifact.sh" \
    --version 1.2.3 \
    --source "$source_dir" \
    --output "$tmp/private-key/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --source-date-epoch 1700000000 \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected private key material to be rejected" >&2
    exit 1
fi
rm "$source_dir/core/leaked.pem"

ln "$source_dir/core/.ai/manifest.yaml" "$source_dir/core/.ai/manifest.hardlink.yaml"
if "$root/scripts/release/package-bundle-artifact.sh" \
    --version 1.2.3 \
    --source "$source_dir" \
    --output "$tmp/hard-link/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --source-date-epoch 1700000000 \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected multiply-linked files to be rejected" >&2
    exit 1
fi
rm "$source_dir/core/.ai/manifest.hardlink.yaml"

sed -i "s/$official_fp/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/" \
    "$source_dir/core/PUBLISHER_TRUST.toml"
if "$root/scripts/release/package-bundle-artifact.sh" \
    --version 1.2.3 \
    --source "$source_dir" \
    --output "$tmp/rejected/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --source-date-epoch 1700000000 \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected non-official publisher metadata to be rejected" >&2
    exit 1
fi
