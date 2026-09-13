#!/usr/bin/env bash

# Cheap ownership regression: no Docker, network, archives, compiler or node.
set -euo pipefail
export LC_ALL=C

root="$(cd "$(dirname "$0")/../../.." && pwd)"
acquire="$root/scripts/release/acquire-development-toolchain-stage0.sh"
producer="$root/.ai/tools/ryeos/development/stage0-platform-production/produce.sh"
contract="$root/.ai/tools/ryeos/development/stage0-platform-production/lib/contract.sh"
dockerfile="$root/Dockerfile.development-realizations"

for source in "$acquire" "$producer" "$contract"; do
    [[ -f "$source" && ! -L "$source" ]]
    bash -n "$source"
done

grep -Fq 'curl --fail' "$acquire"
if grep -Eq '(^|[^a-z])curl([^-a-z]|$)|https://' "$producer"; then
    echo 'offline Stage0 producer retained network acquisition behavior' >&2
    exit 1
fi
grep -Fq 'stage0_contract_load' "$acquire"
grep -Fq 'stage0_contract_load' "$producer"
grep -Fq 'acquire-development-toolchain-stage0.sh' "$dockerfile"
grep -Fq 'stage0-platform-production/produce.sh' "$dockerfile"
grep -Fq -- '--input-root /publisher/acquired' "$dockerfile"
if rg -n 'produce-development-toolchain-stage0\.sh' \
    "$root/Dockerfile.development-realizations" "$root/.ai" \
    "$root/scripts" "$root/tests/e2e/development-toolchain-stage0"; then
    echo 'retired combined Stage0 producer still has an active caller' >&2
    exit 1
fi

# Exercise the closed handoff with tiny fake archives. The producer must accept
# the exact receipt/member inventory far enough to reach archive decoding, then
# refuse an extra member before attempting production. No artifact is claimed.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/acquired/archives" "$tmp/acquired/image" "$tmp/out"
for id in cargo clippy rust_std rustc rustfmt zig patchelf; do
    printf '%s\n' "$id" > "$tmp/acquired/archives/$id.input"
done
component_sha="$(sha256sum "$tmp/acquired/archives/cargo.input" | awk '{print $1}')"
component_bytes="$(stat -c '%s' "$tmp/acquired/archives/cargo.input")"
for id in clippy rust_std rustc rustfmt; do
    cp "$tmp/acquired/archives/cargo.input" "$tmp/acquired/archives/$id.input"
done
rust_base=https://static.rust-lang.org/dist/2026-04-16
for id in cargo clippy rust_std rustc rustfmt; do
    case "$id" in
        rust_std) archive=rust-std-1.95.0-x86_64-unknown-linux-gnu.tar.xz ;;
        *) archive="${id//_/-}-1.95.0-x86_64-unknown-linux-gnu.tar.xz" ;;
    esac
    printf '%s %s\n' "$rust_base/$archive" "$component_sha" >> "$tmp/acquired/archives/rust_manifest.input"
done
manifest_sha="$(sha256sum "$tmp/acquired/archives/rust_manifest.input" | awk '{print $1}')"
manifest_bytes="$(stat -c '%s' "$tmp/acquired/archives/rust_manifest.input")"
zig_sha="$(sha256sum "$tmp/acquired/archives/zig.input" | awk '{print $1}')"
zig_bytes="$(stat -c '%s' "$tmp/acquired/archives/zig.input")"
patchelf_sha="$(sha256sum "$tmp/acquired/archives/patchelf.input" | awk '{print $1}')"
patchelf_bytes="$(stat -c '%s' "$tmp/acquired/archives/patchelf.input")"
printf image > "$tmp/acquired/image/image_member_loader"
chmod 0755 "$tmp/acquired/image/image_member_loader"
image_sha="$(sha256sum "$tmp/acquired/image/image_member_loader" | awk '{print $1}')"
image_bytes="$(stat -c '%s' "$tmp/acquired/image/image_member_loader")"
cat > "$tmp/inputs.yaml" <<EOF
category: "development/ryeos"
name: "stage0-platform-x86_64-linux"
version: "3.0.0"
schema: "ryeos.development.stage0-platform-inputs.v3"
target: "x86_64-unknown-linux-gnu"
source_date_epoch: "1776297600"
publisher_image: "docker.io/library/rust@sha256:443dd9a3260cf23c22fc05051dd5661dd7b4028d3d25dbaffab6563b63c3539c"
rust_version: "1.95.0"
rust_host: "x86_64-unknown-linux-gnu"
rust_manifest_url: "$rust_base/channel-rust-1.95.0.toml"
rust_manifest_sha256: "$manifest_sha"
rust_manifest_bytes: "$manifest_bytes"
cargo_url: "$rust_base/cargo-1.95.0-x86_64-unknown-linux-gnu.tar.xz"
cargo_sha256: "$component_sha"
cargo_bytes: "$component_bytes"
clippy_url: "$rust_base/clippy-1.95.0-x86_64-unknown-linux-gnu.tar.xz"
clippy_sha256: "$component_sha"
clippy_bytes: "$component_bytes"
rust_std_url: "$rust_base/rust-std-1.95.0-x86_64-unknown-linux-gnu.tar.xz"
rust_std_sha256: "$component_sha"
rust_std_bytes: "$component_bytes"
rustc_url: "$rust_base/rustc-1.95.0-x86_64-unknown-linux-gnu.tar.xz"
rustc_sha256: "$component_sha"
rustc_bytes: "$component_bytes"
rustfmt_url: "$rust_base/rustfmt-1.95.0-x86_64-unknown-linux-gnu.tar.xz"
rustfmt_sha256: "$component_sha"
rustfmt_bytes: "$component_bytes"
zig_version: "0.15.2"
zig_url: "https://ziglang.org/download/0.15.2/zig-x86_64-linux-0.15.2.tar.xz"
zig_sha256: "$zig_sha"
zig_bytes: "$zig_bytes"
runtime_mount: "/ryeos/realizations/platform"
patchelf_url: "https://deb.debian.org/debian/pool/main/p/patchelf/patchelf_0.18.0-1.4_amd64.deb"
patchelf_sha256: "$patchelf_sha"
patchelf_bytes: "$patchelf_bytes"
patchelf_program_sha256: "db7d1d1be4a257c75a5bae14e68c2bc825a3be3d2bc13a405436bc56272cfc37"
image_member_loader: "/usr/lib/loader lib/loader 755 $image_bytes $image_sha"
runtime_alias_loader: "lib/loader lib/loader-copy"
output_name: "ryeos-development-toolchain-stage0-rust-1.95.0-zig-0.15.2-x86_64-unknown-linux-gnu.tar.gz"
maximum_output_bytes: "1048576"
maximum_tree_bytes: "1048576"
maximum_tree_entries: "128"
execution_gate: "target_local_binding_and_isolated_acceptance_required"
EOF
contract_sha="$(awk 'NF && $0 !~ /^#/' "$tmp/inputs.yaml" | sha256sum | awk '{print $1}')"
cat > "$tmp/acquired/RYEOS-STAGE0-ACQUISITION" <<EOF
schema=ryeos.development.stage0-acquisition.v1
publisher_image=docker.io/library/rust@sha256:443dd9a3260cf23c22fc05051dd5661dd7b4028d3d25dbaffab6563b63c3539c
input_contract_body_sha256=$contract_sha
EOF
if "$producer" --inputs "$tmp/inputs.yaml" --input-root "$tmp/acquired" \
    --output "$tmp/out/ryeos-development-toolchain-stage0-rust-1.95.0-zig-0.15.2-x86_64-unknown-linux-gnu.tar.gz" \
    > "$tmp/producer-error" 2>&1; then
    echo 'synthetic non-archive unexpectedly produced Stage0' >&2
    exit 1
fi
if grep -Fq 'Stage0 acquisition' "$tmp/producer-error"; then
    cat "$tmp/producer-error" >&2
    echo 'exact synthetic acquisition root did not pass the closed handoff' >&2
    exit 1
fi
printf extra > "$tmp/acquired/extra"
if "$producer" --inputs "$tmp/inputs.yaml" --input-root "$tmp/acquired" \
    --output "$tmp/out/ryeos-development-toolchain-stage0-rust-1.95.0-zig-0.15.2-x86_64-unknown-linux-gnu.tar.gz" \
    > "$tmp/extra-error" 2>&1; then
    echo 'Stage0 producer accepted an extra acquisition member' >&2
    exit 1
fi
grep -Fq 'contains a missing, extra, linked, or special member' "$tmp/extra-error"

echo 'Stage0 acquisition/offline-production ownership checks passed'
