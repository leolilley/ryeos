#!/usr/bin/env bash

# Pre-RyeOS transport boundary for Stage0. It obtains only the archives and
# publisher-image members named by the signed Config, verifies them, and
# atomically publishes one closed input directory. Its immutable identity comes
# from the later retained source/product capture, not filesystem permissions.
# Compilation, transformation, artifact verification and RyeOS import/binding
# are separate.

set -euo pipefail
export LC_ALL=C
umask 022

usage() {
    echo "usage: $0 --inputs FILE --cache DIR --output DIR" >&2
    exit 2
}

inputs=""
cache=""
output=""
while (($#)); do
    case "$1" in
        --inputs) inputs="${2:-}"; shift 2 ;;
        --cache) cache="${2:-}"; shift 2 ;;
        --output) output="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$inputs" && -n "$cache" && -n "$output" ]] || usage

for command in awk bash chmod cp curl dirname install mkdir mktemp mv readlink \
    rm rmdir sed sha256sum sort stat; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "Stage0 acquisition environment is missing required program: $command" >&2
        exit 2
    }
done

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
contract_helper="$repo_root/.ai/tools/ryeos/development/stage0-platform-production/lib/contract.sh"
[[ -f "$contract_helper" && ! -L "$contract_helper" ]]
# shellcheck source=../../.ai/tools/ryeos/development/stage0-platform-production/lib/contract.sh
source "$contract_helper"
stage0_contract_load

[[ "${RYEOS_STAGE0_PUBLISHER_IMAGE:-}" == "$publisher_image" ]] || {
    echo "Stage0 acquisition must run in the exact publisher image selected by its Config" >&2
    exit 2
}

parent="$(dirname "$output")"
[[ -d "$parent" && ! -L "$parent" ]] || {
    echo "Stage0 acquisition output requires an existing ordinary parent" >&2
    exit 2
}
mkdir -p "$cache"
[[ -d "$cache" && ! -L "$cache" ]] || {
    echo "Stage0 download cache must be an ordinary directory" >&2
    exit 2
}
[[ ! -e "$output" ]] || {
    echo "refusing to replace Stage0 acquisition output: $output" >&2
    exit 2
}

output_lock="${output}.publish-lock"
mkdir "$output_lock" 2>/dev/null || {
    echo "another Stage0 acquisition owns the output" >&2
    exit 2
}
stage="$(mktemp -d "$parent/.ryeos-stage0-acquisition.XXXXXX")" || {
    rmdir "$output_lock"
    exit 2
}
download_temps=()
completed=0
cleanup() {
    local status="$1"
    if [[ "$completed" -ne 1 ]]; then
        rm -rf "$stage"
    fi
    if (( ${#download_temps[@]} > 0 )); then
        rm -f "${download_temps[@]}"
    fi
    rmdir "$output_lock"
    return "$status"
}
trap 'cleanup "$?"' EXIT

mkdir "$stage/archives" "$stage/image"

fetch_input() {
    local id="$1"
    local url sha bytes cached download
    url="$(contract_value "${id}_url")"
    sha="$(contract_value "${id}_sha256")"
    bytes="$(contract_value "${id}_bytes")"
    case "$url" in
        https://static.rust-lang.org/*|https://ziglang.org/*|https://deb.debian.org/debian/pool/main/p/patchelf/*) ;;
        *) echo "Stage0 input uses an unauthorized HTTPS origin: $id" >&2; exit 2 ;;
    esac
    [[ "$sha" =~ ^[0-9a-f]{64}$ && "$bytes" =~ ^[1-9][0-9]*$ ]]
    cached="$cache/${sha}-${url##*/}"
    if [[ -e "$cached" ]]; then
        [[ -f "$cached" && ! -L "$cached" \
            && "$(stat -c '%s' "$cached")" == "$bytes" \
            && "$(sha256sum "$cached" | awk '{print $1}')" == "$sha" ]] || {
            echo "cached Stage0 input contradicts $id" >&2
            exit 2
        }
    else
        download="$cache/.${sha}.download.$$"
        download_temps+=("$download")
        rm -f "$download"
        curl --fail --max-redirs 0 --proto '=https' --tlsv1.2 \
            --max-filesize "$bytes" --output "$download" "$url"
        [[ -f "$download" && ! -L "$download" \
            && "$(stat -c '%s' "$download")" == "$bytes" \
            && "$(sha256sum "$download" | awk '{print $1}')" == "$sha" ]] || {
            echo "downloaded Stage0 input contradicts $id" >&2
            exit 2
        }
        mv "$download" "$cached"
    fi
    install -m 0644 "$cached" "$stage/archives/${id}.input"
}

for id in rust_manifest cargo clippy rust_std rustc rustfmt zig patchelf; do
    fetch_input "$id"
done

mapfile -t image_member_keys < <(sed -n 's/^\(image_member_[a-z_]*\): .*/\1/p' "$inputs" | sort)
for key in "${image_member_keys[@]}"; do
    read -r source destination mode bytes sha extra <<< "$(contract_value "$key")"
    [[ -z "$extra" && "$source" == /usr/* \
        && -f "$source" && ! -L "$source" && "$(readlink -f "$source")" == "$source" \
        && "$(stat -c '%a' "$source")" == "$mode" \
        && "$(stat -c '%s' "$source")" == "$bytes" \
        && "$(sha256sum "$source" | awk '{print $1}')" == "$sha" ]] || {
        echo "pinned publisher image member contradicts $key" >&2
        exit 2
    }
    install -m "$mode" "$source" "$stage/image/$key"
done

cat > "$stage/RYEOS-STAGE0-ACQUISITION" <<EOF
schema=ryeos.development.stage0-acquisition.v1
publisher_image=$publisher_image
input_contract_body_sha256=$input_contract_body_sha256
EOF
chmod 0644 "$stage/RYEOS-STAGE0-ACQUISITION"
mv "$stage" "$output"
completed=1

echo "acquired exact Stage0 inputs: $output"
echo "offline production, RyeOS import and binding remain separate"
