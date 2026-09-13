#!/usr/bin/env bash

# Produce the exact compiler payload for the first source-local development
# realization. This remains the explicit pre-RyeOS bootstrap entry while its
# upstream acquisition and publisher-image extraction are being split from the
# canonical offline producer. It is not the final admitted production owner.
# Every downloaded byte comes from the signed input contract. Image members are
# individually pinned authoring inputs; execution-host library discovery is
# never permitted. Reusable runtime transformation and verification already live
# beside the future Stage0 Tool and must not be copied back into this script.

set -euo pipefail
export LC_ALL=C
umask 022

usage() {
    echo "usage: $0 --inputs FILE --cache DIR --output FILE" >&2
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
[[ -f "$inputs" && ! -L "$inputs" ]] || {
    echo "Stage-0 input contract is missing, linked, or not regular: $inputs" >&2
    exit 2
}

for command in ar awk basename bash cat chmod cmp cp curl dirname find grep gzip \
    install mkdir mktemp mv readelf readlink rm rmdir sed sha256sum sort stat \
    tar touch wc; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "Stage-0 publisher image is missing required program: $command" >&2
        exit 2
    }
done

allowed_keys='category name version schema target source_date_epoch publisher_image rust_version rust_host rust_manifest_url rust_manifest_sha256 rust_manifest_bytes cargo_url cargo_sha256 cargo_bytes clippy_url clippy_sha256 clippy_bytes rust_std_url rust_std_sha256 rust_std_bytes rustc_url rustc_sha256 rustc_bytes rustfmt_url rustfmt_sha256 rustfmt_bytes zig_version zig_url zig_sha256 zig_bytes output_name maximum_output_bytes maximum_tree_bytes maximum_tree_entries execution_gate runtime_mount patchelf_url patchelf_sha256 patchelf_bytes patchelf_program_sha256'

contract_value() {
    local wanted="$1"
    local value
    value="$(awk -v wanted="$wanted" '
        $0 ~ "^" wanted ": \"[^\"]*\"$" {
            line=$0
            sub(/^[^:]+: "/, "", line)
            sub(/"$/, "", line)
            print line
            count++
        }
        END { if (count != 1) exit 17 }
    ' "$inputs")" || {
        echo "Stage-0 input contract must contain exactly one quoted $wanted field" >&2
        exit 2
    }
    printf '%s' "$value"
}

while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" || "$line" == \#* ]] && continue
    [[ ! "$line" =~ [[:cntrl:]] ]] || {
        echo "Stage-0 input contract contains a control character" >&2
        exit 2
    }
    [[ "$line" =~ ^([a-z][a-z0-9_]*)\:\ \"[^\"]*\"$ ]] || {
        echo "Stage-0 input contract is not a flat quoted-scalar mapping" >&2
        exit 2
    }
    key="${BASH_REMATCH[1]}"
    if [[ "$key" =~ ^(image_member|runtime_alias)_[a-z_]+$ ]]; then
        contract_value "$key" >/dev/null
        continue
    fi
    case " $allowed_keys " in
        *" $key "*) ;;
        *) echo "Stage-0 input contract contains unknown field: $key" >&2; exit 2 ;;
    esac
done < "$inputs"
for key in $allowed_keys; do
    contract_value "$key" >/dev/null
done

schema="$(contract_value schema)"
target="$(contract_value target)"
epoch="$(contract_value source_date_epoch)"
publisher_image="$(contract_value publisher_image)"
rust_version="$(contract_value rust_version)"
rust_host="$(contract_value rust_host)"
zig_version="$(contract_value zig_version)"
output_name="$(contract_value output_name)"
maximum_output_bytes="$(contract_value maximum_output_bytes)"
maximum_tree_bytes="$(contract_value maximum_tree_bytes)"
maximum_tree_entries="$(contract_value maximum_tree_entries)"
execution_gate="$(contract_value execution_gate)"

[[ "$schema" == ryeos.development.stage0-platform-inputs.v3 ]]
[[ "$(contract_value category)" == development/ryeos ]]
[[ "$(contract_value name)" == stage0-platform-x86_64-linux ]]
[[ "$(contract_value version)" == 3.0.0 ]]
[[ "$target" == x86_64-unknown-linux-gnu && "$rust_host" == "$target" ]]
[[ "$rust_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
[[ "$zig_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
[[ "$epoch" =~ ^[0-9]+$ ]]
[[ "$publisher_image" =~ ^docker\.io/library/rust@sha256:[0-9a-f]{64}$ ]]
rust_manifest_url="$(contract_value rust_manifest_url)"
rust_dist_base="${rust_manifest_url%/*}"
[[ "$rust_manifest_url" == https://static.rust-lang.org/dist/*/channel-rust-${rust_version}.toml \
    && "$(contract_value cargo_url)" == "$rust_dist_base/cargo-${rust_version}-${rust_host}.tar.xz" \
    && "$(contract_value clippy_url)" == "$rust_dist_base/clippy-${rust_version}-${rust_host}.tar.xz" \
    && "$(contract_value rust_std_url)" == "$rust_dist_base/rust-std-${rust_version}-${rust_host}.tar.xz" \
    && "$(contract_value rustc_url)" == "$rust_dist_base/rustc-${rust_version}-${rust_host}.tar.xz" \
    && "$(contract_value rustfmt_url)" == "$rust_dist_base/rustfmt-${rust_version}-${rust_host}.tar.xz" ]] || {
    echo "Stage-0 Rust inputs do not match their exact version/host manifest coordinate" >&2
    exit 2
}
# Discovery catalogs are mutable. Only the versioned archive selected by the
# authored URL/size/digest contract belongs in a reproducible acquisition.
[[ "$(contract_value zig_url)" == "https://ziglang.org/download/${zig_version}/zig-x86_64-linux-${zig_version}.tar.xz" ]] || {
    echo "Stage-0 Zig input does not match its exact version/target coordinate" >&2
    exit 2
}
[[ "${RYEOS_STAGE0_PUBLISHER_IMAGE:-}" == "$publisher_image" ]] || {
    echo "Stage-0 producer must run in the exact publisher image selected by its input contract" >&2
    exit 2
}
[[ "$maximum_output_bytes" =~ ^[1-9][0-9]*$ ]]
[[ "$maximum_tree_bytes" =~ ^[1-9][0-9]*$ ]]
[[ "$maximum_tree_entries" =~ ^[1-9][0-9]*$ ]]
[[ "$execution_gate" == target_local_binding_and_isolated_acceptance_required ]]
repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
runtime_helper="$repo_root/.ai/tools/ryeos/development/stage0-platform-production/lib/runtime.sh"
[[ -f "$runtime_helper" && ! -L "$runtime_helper" ]]
# shellcheck source=../../.ai/tools/ryeos/development/stage0-platform-production/lib/runtime.sh
source "$runtime_helper"
runtime_contract_validate
expected_output_name="ryeos-development-toolchain-stage0-rust-${rust_version}-zig-${zig_version}-${target}.tar.gz"
[[ "$output_name" == "$expected_output_name" ]] || {
    echo "Stage-0 output name does not match its exact toolchain coordinate" >&2
    exit 2
}
[[ "$(basename "$output")" == "$output_name" ]] || {
    echo "Stage-0 output must be named $output_name" >&2
    exit 2
}
mkdir -p "$cache" "$(dirname "$output")"
[[ -d "$cache" && ! -L "$cache" ]] || {
    echo "Stage-0 download cache must be a real directory" >&2
    exit 2
}

output_lock="${output}.publish-lock"
mkdir "$output_lock" 2>/dev/null || {
    echo "another Stage-0 publisher owns the output" >&2
    exit 2
}
[[ ! -e "$output" && ! -e "$output.sha256" ]] || {
    rmdir "$output_lock"
    echo "refusing to overwrite Stage-0 output: $output" >&2
    exit 2
}

tmp="$(mktemp -d)" || {
    rmdir "$output_lock"
    exit 2
}
archive_tmp="$output.tmp.$$"
checksum_tmp="$output.sha256.tmp.$$"
completed=0
download_temps=()
cleanup() {
    local status="$1"
    rm -rf "$tmp"
    if (( ${#download_temps[@]} > 0 )); then
        rm -f "${download_temps[@]}"
    fi
    rm -f "$archive_tmp" "$checksum_tmp"
    if [[ "$completed" -ne 1 ]]; then
        rm -f "$output" "$output.sha256"
    fi
    rmdir "$output_lock"
    return "$status"
}
trap 'cleanup "$?"' EXIT

fetch_input() {
    local id="$1"
    local url sha bytes destination download
    url="$(contract_value "${id}_url")"
    sha="$(contract_value "${id}_sha256")"
    bytes="$(contract_value "${id}_bytes")"
    case "$url" in
        https://static.rust-lang.org/*|https://ziglang.org/*|https://deb.debian.org/debian/pool/main/p/patchelf/*) ;;
        *) echo "Stage-0 input uses an unauthorized HTTPS origin: $id" >&2; exit 2 ;;
    esac
    [[ "$sha" =~ ^[0-9a-f]{64}$ && "$bytes" =~ ^[1-9][0-9]*$ ]]
    destination="$cache/${sha}-${url##*/}"
    if [[ -e "$destination" ]]; then
        [[ -f "$destination" && ! -L "$destination" \
            && "$(stat -c '%s' "$destination")" == "$bytes" \
            && "$(sha256sum "$destination" | awk '{print $1}')" == "$sha" ]] || {
            echo "cached Stage-0 input contradicts $id" >&2
            exit 2
        }
    else
        download="$cache/.${sha}.download.$$"
        download_temps+=("$download")
        rm -f "$download"
        # The signed contract names the exact origin as well as the digest.
        # Redirects would silently add an undeclared network destination.
        curl --fail --max-redirs 0 --proto '=https' --tlsv1.2 \
            --max-filesize "$bytes" --output "$download" "$url"
        [[ -f "$download" && ! -L "$download" \
            && "$(stat -c '%s' "$download")" == "$bytes" \
            && "$(sha256sum "$download" | awk '{print $1}')" == "$sha" ]] || {
            echo "downloaded Stage-0 input contradicts $id" >&2
            exit 2
        }
        mv "$download" "$destination"
    fi
    cp "$destination" "$tmp/${id}.input"
}

for id in rust_manifest cargo clippy rust_std rustc rustfmt zig patchelf; do
    fetch_input "$id"
done

for id in cargo clippy rust_std rustc rustfmt; do
    grep -Fq "$(contract_value "${id}_url")" "$tmp/rust_manifest.input"
    grep -Fq "$(contract_value "${id}_sha256")" "$tmp/rust_manifest.input"
done

stage="$tmp/tree"
mkdir "$stage"
for id in cargo clippy rust_std rustc rustfmt; do
    component="$tmp/component-$id"
    mkdir "$component"
    tar -xJf "$tmp/${id}.input" --no-same-owner -C "$component"
    mapfile -t roots < <(find "$component" -mindepth 1 -maxdepth 1 -type d -print | sort)
    [[ "${#roots[@]}" -eq 1 && -x "${roots[0]}/install.sh" ]] || {
        echo "Rust component $id does not contain one installer root" >&2
        exit 2
    }
    "${roots[0]}/install.sh" \
        --prefix=/rust \
        --destdir="$stage" \
        --disable-ldconfig
    if [[ "$id" == rustc ]]; then
        # The component installer omits the archive-level Rust notices. Keep
        # them from the exact verified archive, not from the publisher image.
        for notice in COPYRIGHT LICENSE-APACHE LICENSE-MIT; do
            [[ -f "${roots[0]}/$notice" && ! -L "${roots[0]}/$notice" ]] || {
                echo "Rust archive is missing required notice: $notice" >&2
                exit 2
            }
            install -D -m 0644 "${roots[0]}/$notice" "$stage/rust/share/doc/rust/$notice"
        done
    fi
done

# Installer state records random destdir paths and offers mutation operations
# that do not belong in an immutable realization. Remove only this exact
# bookkeeping set; compiler files, libraries and notices remain untouched.
for metadata in components install.log rust-installer-version uninstall.sh \
    manifest-cargo manifest-clippy-preview "manifest-rust-std-$rust_host" \
    manifest-rustc manifest-rustfmt-preview; do
    path="$stage/rust/lib/rustlib/$metadata"
    [[ -f "$path" && ! -L "$path" ]] || {
        echo "Rust installer bookkeeping does not match the selected components: $metadata" >&2
        exit 2
    }
    rm -- "$path"
done

zig_extract="$tmp/zig-extract"
mkdir "$zig_extract"
tar -xJf "$tmp/zig.input" --no-same-owner -C "$zig_extract"
zig_root="$zig_extract/zig-x86_64-linux-$zig_version"
[[ -d "$zig_root" && ! -L "$zig_root" && -x "$zig_root/zig" ]]
[[ "$(find "$zig_extract" -mindepth 1 -maxdepth 1 | wc -l)" -eq 1 ]]
mv "$zig_root" "$stage/zig"

runtime_assemble "$stage" "$tmp" "$tmp/patchelf.input"

cp "$tmp/rust_manifest.input" "$stage/UPSTREAM-RUST-MANIFEST.toml"
inputs_sha="$(awk 'NF && $0 !~ /^#/' "$inputs" | sha256sum | awk '{print $1}')"
producer_sha="$(sha256sum "$0" | awk '{print $1}')"
cat > "$stage/RYEOS-BOOTSTRAP" <<EOF
schema=ryeos.development-toolchain-bootstrap.v2
artifact_class=runtime_closed_platform_candidate
execution_gate=$execution_gate
target=$target
rust_version=$rust_version
rust_host=$rust_host
zig_version=$zig_version
source_date_epoch=$epoch
publisher_image=$publisher_image
input_contract_body_sha256=$inputs_sha
producer_sha256=$producer_sha
runtime_helper_sha256=$(sha256sum "$runtime_helper" | awk '{print $1}')
EOF

programs="$stage/RYEOS-AUTHORING-PROGRAMS"
: > "$programs"
for command in ar awk basename bash cat chmod cmp cp curl dirname find grep gzip \
    install mkdir mktemp mv readelf readlink rm rmdir sed sha256sum sort stat \
    tar touch wc; do
    path="$(command -v "$command")"
    printf '%s  %s\n' "$(sha256sum "$path" | awk '{print $1}')" "$path" >> "$programs"
done

printf '%s  %s\n' "$(contract_value patchelf_program_sha256)" /authoring/patchelf >> "$programs"

# Inventory all final loader/library edges after the shared verifier has
# checked their recursive closure. This is evidence, not RyeOS launch authority.
runtime_dependencies="$stage/RYEOS-RUNTIME-DEPENDENCIES"
: > "$runtime_dependencies"
while IFS= read -r -d '' executable; do
    relative="${executable#"$stage/"}"
    [[ ! "$relative" =~ [[:cntrl:]] ]]
    # Shared libraries need not have an executable mode bit. Inspect every
    # regular file so their own DT_NEEDED edges cannot disappear from the
    # closure testimony; ordinary non-executable data is not a program.
    # readelf also accepts .a/.rlib archives, which are linker inputs rather
    # than loadable ELF files. Read at most four bytes (or the first NUL)
    # before inspecting a loadable file, and fail on malformed ELF.
    elf_magic=""
    IFS= read -r -n 4 -d '' elf_magic < "$executable" || true
    if [[ "$elf_magic" == $'\177ELF' ]]; then
        readelf -h "$executable" >/dev/null
        executable_sha="$(sha256sum "$executable" | awk '{print $1}')"
        interpreter="$(readelf -l "$executable" | sed -n 's/.*Requesting program interpreter: \([^]]*\)].*/\1/p')"
        needed="$(readelf -d "$executable" | sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p' | sort | awk 'BEGIN { first=1 } { if (!first) printf ","; printf "%s", $0; first=0 } END { printf "\n" }')"
        printf '%s\t%s\telf\t%s\t%s\n' "$executable_sha" "$relative" "${interpreter:--}" "${needed:--}" >> "$runtime_dependencies"
    elif [[ -x "$executable" ]]; then
        executable_sha="$(sha256sum "$executable" | awk '{print $1}')"
        printf '%s\t%s\tnon_elf\t-\t-\n' "$executable_sha" "$relative" >> "$runtime_dependencies"
    fi
done < <(find "$stage/rust" "$stage/zig" "$stage/lib" "$stage/native" -type f -print0 | sort -z)

while IFS= read -r -d '' entry; do
    relative="${entry#"$stage/"}"
    [[ ! "$relative" =~ [[:cntrl:]] ]] || {
        echo "Stage-0 output contains a non-portable path" >&2
        exit 2
    }
    [[ ! -L "$entry" ]] || {
        echo "Stage-0 output contains a symbolic link: $relative" >&2
        exit 2
    }
    [[ -d "$entry" || -f "$entry" ]] || {
        echo "Stage-0 output contains a special filesystem entry: $relative" >&2
        exit 2
    }
    [[ ! -f "$entry" || "$(stat -c '%h' "$entry")" -eq 1 ]] || {
        echo "Stage-0 output contains a multiply-linked file: $relative" >&2
        exit 2
    }
    mode="$(stat -c '%a' "$entry")"
    [[ "$mode" =~ ^[0-7]{3}$ ]] || {
        echo "Stage-0 output contains special permission bits: $relative" >&2
        exit 2
    }
    mode_value=$((8#$mode))
    (( (mode_value & 0022) == 0 )) || {
        echo "Stage-0 output contains a group/other-writable entry: $relative" >&2
        exit 2
    }
done < <(find "$stage" -mindepth 1 -print0 | sort -z)

tree_manifest="$stage/RYEOS-TREE-SHA256"
: > "$tree_manifest"
while IFS= read -r -d '' entry; do
    relative="${entry#"$stage/"}"
    [[ "$relative" != RYEOS-TREE-SHA256 ]] || continue
    mode="$(stat -c '%a' "$entry")"
    if [[ -d "$entry" ]]; then
        printf 'd\t%s\t-\t%s\n' "$mode" "$relative" >> "$tree_manifest"
    else
        printf 'f\t%s\t%s\t%s\n' "$mode" "$(sha256sum "$entry" | awk '{print $1}')" "$relative" >> "$tree_manifest"
    fi
done < <(find "$stage" -mindepth 1 -print0 | sort -z)

tree_bytes="$(find "$stage" -type f -printf '%s\n' | awk '{total += $1} END {print total + 0}')"
tree_entries="$(find "$stage" -mindepth 1 -printf '.\n' | awk 'END {print NR + 0}')"
(( tree_bytes <= maximum_tree_bytes && tree_entries <= maximum_tree_entries )) || {
    echo "Stage-0 tree exceeds its signed entry or byte bound" >&2
    exit 2
}

find "$stage" -depth -exec touch -h -d "@$epoch" {} +
root_name="${output_name%.tar.gz}"
mv "$stage" "$tmp/$root_name"
tar \
    --sort=name \
    --format=posix \
    --pax-option=delete=atime,delete=ctime \
    --mtime="@$epoch" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$tmp" \
    -cf - "$root_name" \
    | gzip -n -9 > "$archive_tmp"
(( $(stat -c '%s' "$archive_tmp") <= maximum_output_bytes )) || {
    echo "Stage-0 archive exceeds its signed byte bound" >&2
    exit 2
}
mv "$archive_tmp" "$output"
(
    cd "$(dirname "$output")"
    sha256sum "$(basename "$output")"
) > "$checksum_tmp"
mv "$checksum_tmp" "$output.sha256"
completed=1

echo "produced exact Stage-0 runtime-closed platform candidate: $output"
echo "execution remains gated on target-local binding and isolated acceptance"
