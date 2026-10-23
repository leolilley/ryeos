#!/usr/bin/env bash

# Hermetic regression cases for materialize-release-bundles.sh. The fixture
# archive carries the official fingerprint in its trust documents; the
# negatives each break exactly one property of a good archive.

set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

script="$root/scripts/release/materialize-release-bundles.sh"
official_fp="$("$root/scripts/release/official-publisher-fingerprint.sh")"
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"

version=1.2.3
archive_name="ryeos-bundles-${version}-x86_64.tar.gz"
archive_root="ryeos-bundles-${version}-x86_64"

write_trust_doc() {
    local path="$1" fingerprint="${2:-$official_fp}"
    mkdir -p "$(dirname "$path")"
    cat > "$path" <<EOT
public_key = "ed25519:test-fixture"
fingerprint = "$fingerprint"
owner = "ryeos-official"
EOT
}

tree="$tmp/tree"
fixture="$tree/$archive_root"
mkdir -p "$fixture/.ai/node/init/profiles"
write_trust_doc "$fixture/.ai/PUBLISHER_TRUST.toml"
while IFS= read -r bundle; do
    mkdir -p "$fixture/$bundle/.ai"
    printf 'name: %s\n' "$bundle" > "$fixture/$bundle/.ai/manifest.yaml"
    write_trust_doc "$fixture/$bundle/PUBLISHER_TRUST.toml"
done < <(ryeos_release_archive_bundle_names)
while IFS= read -r node_init_profile; do
    {
        printf 'schema: 1\nexact_bundles:\n'
        ryeos_bundle_set_names "$(ryeos_node_init_profile_bundle_set "$node_init_profile")" \
            | sort | sed 's/^/  - /'
        printf '%s\n' 'policies:' '  fixture_policy:' '    schema: 1'
    } > "$fixture/.ai/node/init/profiles/$node_init_profile.yaml"
done < <(ryeos_node_init_profile_names)

# pack NAME: archive the current fixture tree under $tmp/NAME with a checksum.
pack() {
    local dir="$tmp/$1"
    mkdir -p "$dir"
    tar -C "$tree" -czf "$dir/$archive_name" "$archive_root"
    (cd "$dir" && sha256sum "$archive_name" > "$archive_name.sha256")
}

run() {
    local name="$1" out="$2"
    "$script" --version "$version" \
        --archive "$tmp/$name/$archive_name" \
        --checksum "$tmp/$name/$archive_name.sha256" \
        --set f5r-controller --output "$out"
}

expect_rejected() {
    local why="$1"
    shift
    if "$@" >/dev/null 2>&1; then
        echo "expected rejection: $why" >&2
        exit 1
    fi
}

# Positive: the source-root .ai plus exactly the controller set's bundles.
pack good
run good "$tmp/out-good" >/dev/null
actual="$(find "$tmp/out-good" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)"
expected="$(printf '%s\n' .ai codex core graph knowledge render-sandbox | sort)"
[[ "$actual" == "$expected" ]] || {
    echo "unexpected materialized layout: $actual" >&2
    exit 1
}
[[ -f "$tmp/out-good/render-sandbox/.ai/manifest.yaml" ]]
[[ -f "$tmp/out-good/.ai/PUBLISHER_TRUST.toml" ]]
[[ ! -e "$tmp/out-good/web" && ! -e "$tmp/out-good/directive" ]]

# An output directory that exists and is empty is fine.
mkdir "$tmp/out-empty"
run good "$tmp/out-empty" >/dev/null
[[ -d "$tmp/out-empty/core/.ai" ]]

# Negative: non-empty output.
mkdir "$tmp/out-busy"
touch "$tmp/out-busy/leftover"
expect_rejected "non-empty output" run good "$tmp/out-busy"
[[ ! -e "$tmp/out-busy/core" ]]

# Negative: unknown bundle set.
expect_rejected "unknown set" "$script" --version "$version" \
    --archive "$tmp/good/$archive_name" --checksum "$tmp/good/$archive_name.sha256" \
    --set no-such-set --output "$tmp/out-unknown"

# Negative: checksum mismatch.
pack badsum
printf '%s  %s\n' "$(printf '0%.0s' {1..64})" "$archive_name" > "$tmp/badsum/$archive_name.sha256"
expect_rejected "checksum mismatch" run badsum "$tmp/out-badsum"

# Negative: archive name does not match the version.
mkdir "$tmp/badname"
cp "$tmp/good/$archive_name" "$tmp/badname/ryeos-bundles-9.9.9-x86_64.tar.gz"
cp "$tmp/good/$archive_name.sha256" "$tmp/badname/ryeos-bundles-9.9.9-x86_64.tar.gz.sha256"
expect_rejected "archive name" "$script" --version "$version" \
    --archive "$tmp/badname/ryeos-bundles-9.9.9-x86_64.tar.gz" \
    --checksum "$tmp/badname/ryeos-bundles-9.9.9-x86_64.tar.gz.sha256" \
    --set f5r-controller --output "$tmp/out-badname"

# Negative: render-sandbox missing from the archive.
mv "$fixture/render-sandbox" "$tmp/render-sandbox.held"
pack nosandbox
expect_rejected "missing render-sandbox" run nosandbox "$tmp/out-nosandbox"
mv "$tmp/render-sandbox.held" "$fixture/render-sandbox"

# Negative: a non-official trust document in a materialized bundle.
write_trust_doc "$fixture/render-sandbox/PUBLISHER_TRUST.toml" \
    "$(printf 'f%.0s' {1..64})"
pack foreign
expect_rejected "non-official trust document" run foreign "$tmp/out-foreign"
write_trust_doc "$fixture/render-sandbox/PUBLISHER_TRUST.toml"

# Negative: a private key file anywhere in the archive.
printf '%s\n' placeholder > "$fixture/core/leaked.pem"
pack pem
expect_rejected "pem file" run pem "$tmp/out-pem"
rm "$fixture/core/leaked.pem"

# The repaired tree still materializes.
pack again
run again "$tmp/out-again" >/dev/null
