#!/usr/bin/env bash

# Canonical offline producer for the exact Stage0 compiler platform. This file
# owns transformation and archive production for both the pre-RyeOS publisher
# and a later admitted Stage1 Tool. It never downloads, inspects an execution
# host, or resolves an executable outside its admitted process environment.

set -euo pipefail
export LC_ALL=C
umask 022

usage() {
    echo "usage: $0 --inputs FILE --input-root DIR --output FILE" >&2
    exit 2
}

inputs=""
input_root=""
output=""
while (($#)); do
    case "$1" in
        --inputs) inputs="${2:-}"; shift 2 ;;
        --input-root) input_root="${2:-}"; shift 2 ;;
        --output) output="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$inputs" && -n "$input_root" && -n "$output" ]] || usage

for command in ar awk basename bash cat chmod cmp cp dirname find grep gzip \
    install mkdir mktemp mv readelf readlink rm rmdir sed sha256sum sort stat \
    tar touch wc; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "Stage0 offline producer is missing admitted program: $command" >&2
        exit 2
    }
done

producer_dir="$(cd "$(dirname "$0")" && pwd)"
contract_helper="$producer_dir/lib/contract.sh"
runtime_helper="$producer_dir/lib/runtime.sh"
[[ -f "$contract_helper" && ! -L "$contract_helper" ]]
[[ -f "$runtime_helper" && ! -L "$runtime_helper" ]]
# shellcheck source=lib/contract.sh
source "$contract_helper"
stage0_contract_load
# shellcheck source=lib/runtime.sh
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
[[ -d "$input_root" && ! -L "$input_root" \
    && -d "$input_root/archives" && ! -L "$input_root/archives" \
    && -d "$input_root/image" && ! -L "$input_root/image" ]] || {
    echo "Stage0 offline producer requires one ordinary acquisition input root" >&2
    exit 2
}
mkdir -p "$(dirname "$output")"

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
cleanup() {
    local status="$1"
    rm -rf "$tmp"
    rm -f "$archive_tmp" "$checksum_tmp"
    if [[ "$completed" -ne 1 ]]; then
        rm -f "$output" "$output.sha256"
    fi
    rmdir "$output_lock"
    return "$status"
}
trap 'cleanup "$?"' EXIT

expected_members="$tmp/acquisition-members.expected"
actual_members="$tmp/acquisition-members.actual"
printf 'd\tarchives\nd\timage\nf\tRYEOS-STAGE0-ACQUISITION\n' > "$expected_members"
for id in rust_manifest cargo clippy rust_std rustc rustfmt zig patchelf; do
    member="$input_root/archives/${id}.input"
    sha="$(contract_value "${id}_sha256")"
    bytes="$(contract_value "${id}_bytes")"
    [[ -f "$member" && ! -L "$member" \
        && "$(stat -c '%a' "$member")" == 644 \
        && "$(stat -c '%s' "$member")" == "$bytes" \
        && "$(sha256sum "$member" | awk '{print $1}')" == "$sha" ]] || {
        echo "Stage0 acquisition archive contradicts $id" >&2
        exit 2
    }
    printf 'f\tarchives/%s.input\n' "$id" >> "$expected_members"
done
mapfile -t image_member_keys < <(sed -n 's/^\(image_member_[a-z_]*\): .*/\1/p' "$inputs" | sort)
for key in "${image_member_keys[@]}"; do
    read -r source destination mode bytes sha extra <<< "$(contract_value "$key")"
    member="$input_root/image/$key"
    [[ -z "$extra" && -f "$member" && ! -L "$member" \
        && "$(stat -c '%a' "$member")" == "$mode" \
        && "$(stat -c '%s' "$member")" == "$bytes" \
        && "$(sha256sum "$member" | awk '{print $1}')" == "$sha" ]] || {
        echo "Stage0 acquisition image member contradicts $key" >&2
        exit 2
    }
    printf 'f\timage/%s\n' "$key" >> "$expected_members"
done
find "$input_root" -mindepth 1 -printf '%y\t%P\n' | sort > "$actual_members"
sort -o "$expected_members" "$expected_members"
cmp "$expected_members" "$actual_members" || {
    echo "Stage0 acquisition root contains a missing, extra, linked, or special member" >&2
    exit 2
}
cat > "$tmp/acquisition-receipt.expected" <<EOF
schema=ryeos.development.stage0-acquisition.v1
publisher_image=$publisher_image
input_contract_body_sha256=$input_contract_body_sha256
EOF
cmp "$tmp/acquisition-receipt.expected" "$input_root/RYEOS-STAGE0-ACQUISITION" || {
    echo "Stage0 acquisition receipt does not name the exact Config and publisher image" >&2
    exit 2
}

for id in cargo clippy rust_std rustc rustfmt; do
    grep -Fq "$(contract_value "${id}_url")" "$input_root/archives/rust_manifest.input"
    grep -Fq "$(contract_value "${id}_sha256")" "$input_root/archives/rust_manifest.input"
done

stage="$tmp/tree"
mkdir "$stage"
for id in cargo clippy rust_std rustc rustfmt; do
    component="$tmp/component-$id"
    mkdir "$component"
    tar -xJf "$input_root/archives/${id}.input" --no-same-owner -C "$component"
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
tar -xJf "$input_root/archives/zig.input" --no-same-owner -C "$zig_extract"
zig_root="$zig_extract/zig-x86_64-linux-$zig_version"
[[ -d "$zig_root" && ! -L "$zig_root" && -x "$zig_root/zig" ]]
[[ "$(find "$zig_extract" -mindepth 1 -maxdepth 1 | wc -l)" -eq 1 ]]
mv "$zig_root" "$stage/zig"

runtime_assemble "$stage" "$tmp" "$input_root/archives/patchelf.input" "$input_root/image"

cp "$input_root/archives/rust_manifest.input" "$stage/UPSTREAM-RUST-MANIFEST.toml"
inputs_sha="$input_contract_body_sha256"
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
for command in ar awk basename bash cat chmod cmp cp dirname find grep gzip \
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
