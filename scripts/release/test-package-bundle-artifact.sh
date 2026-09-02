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
mkdir -p "$source_dir/.ai/node/init/profiles"
while IFS= read -r node_init_profile; do
    {
        printf 'schema: 1\nexact_bundles:\n'
        ryeos_bundle_set_names "$node_init_profile" | sort | sed 's/^/  - /'
        # This fixture exercises shell packaging structure. The fake RyeOS
        # below is its type oracle; production packaging uses the real release
        # binary and therefore the live Rust node-policy registry/cardinalities.
        printf '%s\n' 'policies:' '  fixture_policy:' '    schema: 1'
    } > "$source_dir/.ai/node/init/profiles/$node_init_profile.yaml"
done < <(ryeos_node_init_profile_names)

fake_ryeos="$tmp/ryeos"
cat > "$fake_ryeos" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ " $* " != *" --trust-file "* ]] || {
    echo "release verification must not accept a trust-file override" >&2
    exit 2
}
[[ "${1:-}" == init ]]
shift
app_root=""
source_dir=""
node_profile=""
while (( $# > 0 )); do
    case "$1" in
        --non-interactive) shift ;;
        --app-root) app_root="${2:-}"; shift 2 ;;
        --source) source_dir="${2:-}"; shift 2 ;;
        --node-profile) node_profile="${2:-}"; shift 2 ;;
        *) echo "unexpected init argument: $1" >&2; exit 2 ;;
    esac
done
[[ -n "$app_root" && -n "$source_dir" && -d "$source_dir/.ai" ]]
mkdir -p "$app_root"
printf '%s\n' "${node_profile:-none}" >> "${FAKE_RYEOS_CALLS:?}"
[[ -n "$node_profile" ]] || exit 2
if [[ -n "$node_profile" ]]; then
    profile="$source_dir/.ai/node/init/profiles/$node_profile.yaml"
    [[ -f "$profile" ]]
    expected_bundles="$(
        sed -n '/^exact_bundles:/,/^policies:/p' "$profile" \
            | sed -nE 's/^  - ([A-Za-z0-9_-]+)$/\1/p' \
            | sort
    )"
    actual_bundles="$(
        find "$source_dir" -mindepth 1 -maxdepth 1 -type d -exec test -d '{}/.ai' \; -printf '%f\n' \
            | sort
    )"
    [[ -n "$expected_bundles" && "$actual_bundles" == "$expected_bundles" ]]
    mkdir -p "$app_root/.ai/node/policies"
    while IFS= read -r policy_name; do
        printf 'schema: 1\n' > "$app_root/.ai/node/policies/$policy_name.yaml"
    done < <(
        sed -n '/^policies:/,$p' "$profile" \
            | sed -nE 's/^  ([A-Za-z0-9_-]+):$/\1/p'
    )
fi
EOF
chmod +x "$fake_ryeos"

for run in one two; do
    mkdir -p "$tmp/$run"
    FAKE_RYEOS_CALLS="$tmp/$run/init-calls" \
      "$root/scripts/release/package-bundle-artifact.sh" \
        --version 1.2.3 \
        --source "$source_dir" \
        --output "$tmp/$run/ryeos-bundles-1.2.3-x86_64.tar.gz" \
        --source-date-epoch 1700000000 \
        --ryeos-bin "$fake_ryeos" >/dev/null
    printf '%s\n' none full > "$tmp/$run/expected-init-calls"
    cmp "$tmp/$run/expected-init-calls" "$tmp/$run/init-calls"
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
tar -tzf "$tmp/one/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    | grep -qx 'ryeos-bundles-1.2.3-x86_64/.ai/node/init/profiles/hosted-workflow.yaml'

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

retry_missing_profile="$tmp/retry-missing-profile"
mkdir -p "$retry_missing_profile/tree"
tar -xzf "$tmp/one/ryeos-bundles-1.2.3-x86_64.tar.gz" -C "$retry_missing_profile/tree"
rm "$retry_missing_profile/tree/ryeos-bundles-1.2.3-x86_64/.ai/node/init/profiles/hosted-workflow.yaml"
missing_profile_archive="$retry_missing_profile/ryeos-bundles-1.2.3-x86_64.tar.gz"
tar -C "$retry_missing_profile/tree" -czf "$missing_profile_archive" ryeos-bundles-1.2.3-x86_64
(
    cd "$retry_missing_profile"
    sha256sum "$(basename "$missing_profile_archive")" > "$(basename "$missing_profile_archive").sha256"
)
if "$root/scripts/release/verify-bundle-artifact.sh" \
    --version 1.2.3 \
    --archive "$missing_profile_archive" \
    --checksum "$missing_profile_archive.sha256" >/dev/null 2>&1; then
    echo "expected retry verification to reject a missing node init profile" >&2
    exit 1
fi

retry_extra_profile="$tmp/retry-extra-profile"
mkdir -p \
    "$retry_extra_profile/tree/ryeos-bundles-1.2.3-x86_64/.ai/node/init/profiles/unexpected"
tar -xzf "$tmp/one/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    -C "$retry_extra_profile/tree"
extra_profile_archive="$retry_extra_profile/ryeos-bundles-1.2.3-x86_64.tar.gz"
tar -C "$retry_extra_profile/tree" -czf "$extra_profile_archive" \
    ryeos-bundles-1.2.3-x86_64
(
    cd "$retry_extra_profile"
    sha256sum "$(basename "$extra_profile_archive")" > "$(basename "$extra_profile_archive").sha256"
)
if "$root/scripts/release/verify-bundle-artifact.sh" \
    --version 1.2.3 \
    --archive "$extra_profile_archive" \
    --checksum "$extra_profile_archive.sha256" >/dev/null 2>&1; then
    echo "expected retry verification to reject an extra node init-profile entry" >&2
    exit 1
fi

source_node_init_profile="$source_dir/.ai/node/init/profiles/hosted-workflow.yaml"
mv "$source_node_init_profile" "$tmp/hosted-workflow.yaml"
if FAKE_RYEOS_CALLS="$tmp/missing-source-profile-calls" \
    "$root/scripts/release/package-bundle-artifact.sh" \
    --version 1.2.3 \
    --source "$source_dir" \
    --output "$tmp/missing-source-profile/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --source-date-epoch 1700000000 \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected packaging to reject a missing node init profile" >&2
    exit 1
fi
mv "$tmp/hosted-workflow.yaml" "$source_node_init_profile"

source_contract_profile="$source_dir/.ai/node/init/profiles/standard.yaml"
cp "$source_contract_profile" "$tmp/standard.yaml"
sed -i '/^exact_bundles:$/a\  - unexpected' "$source_contract_profile"
if FAKE_RYEOS_CALLS="$tmp/mismatched-source-profile-calls" \
    "$root/scripts/release/package-bundle-artifact.sh" \
    --version 1.2.3 \
    --source "$source_dir" \
    --output "$tmp/mismatched-source-profile/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --source-date-epoch 1700000000 \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected packaging to reject a mismatched node init profile" >&2
    exit 1
fi
mv "$tmp/standard.yaml" "$source_contract_profile"

mkdir "$source_dir/.ai/node/init/legacy-seed"
if FAKE_RYEOS_CALLS="$tmp/legacy-init-input-calls" \
    "$root/scripts/release/package-bundle-artifact.sh" \
    --version 1.2.3 \
    --source "$source_dir" \
    --output "$tmp/legacy-init-input/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --source-date-epoch 1700000000 \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected packaging to reject a parallel legacy init input" >&2
    exit 1
fi
rmdir "$source_dir/.ai/node/init/legacy-seed"

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
if FAKE_RYEOS_CALLS="$tmp/private-key-init-calls" \
    "$root/scripts/release/package-bundle-artifact.sh" \
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
if FAKE_RYEOS_CALLS="$tmp/hard-link-init-calls" \
    "$root/scripts/release/package-bundle-artifact.sh" \
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
if FAKE_RYEOS_CALLS="$tmp/rejected-init-calls" \
    "$root/scripts/release/package-bundle-artifact.sh" \
    --version 1.2.3 \
    --source "$source_dir" \
    --output "$tmp/rejected/ryeos-bundles-1.2.3-x86_64.tar.gz" \
    --source-date-epoch 1700000000 \
    --ryeos-bin "$fake_ryeos" >/dev/null 2>&1; then
    echo "expected non-official publisher metadata to be rejected" >&2
    exit 1
fi
