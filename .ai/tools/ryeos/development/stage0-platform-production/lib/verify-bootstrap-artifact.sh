#!/usr/bin/env bash
# ryeos:signed:2026-09-13T08:57:58Z:e712b85d413b5b103736b9d580947f41fff9d852ad9c9ee6b76af311cfe34637:28Y0498/gvoedRPhFN2gztlOtK6/UxbinX4iqrUpkIFIfnrZbft3O4Jwhedt18lOJnykKfjlf2ZfHfuoCPNMDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea

# Verify and optionally materialize one Stage-0 development compiler payload.
# This verifies transport and publisher testimony only. RyeOS's ordinary
# external-content manifest/import and target-local consumer binding remain the
# execution authority.

set -euo pipefail
export LC_ALL=C
umask 022

usage() {
    echo "usage: $0 --inputs FILE --producer FILE --archive FILE [--checksum FILE] [--materialize DIR]" >&2
    exit 2
}

inputs=""
producer=""
archive=""
checksum=""
materialize=""
materialize_lock=""
while (($#)); do
    case "$1" in
        --inputs) inputs="${2:-}"; shift 2 ;;
        --producer) producer="${2:-}"; shift 2 ;;
        --archive) archive="${2:-}"; shift 2 ;;
        --checksum) checksum="${2:-}"; shift 2 ;;
        --materialize) materialize="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$inputs" && -n "$producer" && -n "$archive" ]] || usage
for path in "$inputs" "$producer" "$archive"; do
    [[ -f "$path" && ! -L "$path" ]] || {
        echo "Stage-0 verification input is missing, linked, or not regular: $path" >&2
        exit 2
    }
done
for command in awk basename cat cmp dirname find grep mkdir mktemp mv readelf rm rmdir \
    sed sha256sum sort stat tar uniq wc; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "Stage-0 verification requires $command" >&2
        exit 2
    }
done

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

allowed_keys='category name version schema target source_date_epoch publisher_image rust_version rust_host rust_manifest_url rust_manifest_sha256 rust_manifest_bytes cargo_url cargo_sha256 cargo_bytes clippy_url clippy_sha256 clippy_bytes rust_std_url rust_std_sha256 rust_std_bytes rustc_url rustc_sha256 rustc_bytes rustfmt_url rustfmt_sha256 rustfmt_bytes zig_version zig_url zig_sha256 zig_bytes output_name maximum_output_bytes maximum_tree_bytes maximum_tree_entries execution_gate runtime_mount patchelf_url patchelf_sha256 patchelf_bytes patchelf_program_sha256'
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
[[ "$(contract_value zig_url)" == "https://ziglang.org/download/${zig_version}/zig-x86_64-linux-${zig_version}.tar.xz" ]] || {
    echo "Stage-0 Zig input does not match its exact version/target coordinate" >&2
    exit 2
}
[[ "$maximum_output_bytes" =~ ^[1-9][0-9]*$ ]]
[[ "$maximum_tree_bytes" =~ ^[1-9][0-9]*$ ]]
[[ "$maximum_tree_entries" =~ ^[1-9][0-9]*$ ]]
[[ "$execution_gate" == target_local_binding_and_isolated_acceptance_required ]]
runtime_helper="$(dirname "$0")/runtime.sh"
[[ -f "$runtime_helper" && ! -L "$runtime_helper" ]]
# shellcheck source=runtime.sh
source "$runtime_helper"
runtime_contract_validate
expected_output_name="ryeos-development-toolchain-stage0-rust-${rust_version}-zig-${zig_version}-${target}.tar.gz"
[[ "$output_name" == "$expected_output_name" ]] || {
    echo "Stage-0 output name does not match its exact toolchain coordinate" >&2
    exit 2
}
[[ "$(basename "$archive")" == "$output_name" ]] || {
    echo "unexpected Stage-0 archive name" >&2
    exit 2
}
(( $(stat -c '%s' "$archive") <= maximum_output_bytes )) || {
    echo "Stage-0 archive exceeds its signed byte bound" >&2
    exit 2
}
if [[ -n "$checksum" ]]; then
    [[ -f "$checksum" && ! -L "$checksum" \
        && "$(basename "$checksum")" == "$output_name.sha256" ]] || {
        echo "Stage-0 checksum is missing, linked, or not regular" >&2
        exit 2
    }
    [[ "$(wc -l < "$checksum")" -eq 1 ]] || {
        echo "Stage-0 checksum must contain exactly one entry" >&2
        exit 2
    }
    checksum_line="$(cat "$checksum")"
    read -r expected_digest expected_name extra < "$checksum"
    actual_digest="$(sha256sum "$archive" | awk '{print $1}')"
    [[ -z "${extra:-}" \
        && "$expected_digest" =~ ^[0-9a-f]{64}$ \
        && "$expected_name" == "$output_name" \
        && "$checksum_line" == "$expected_digest  $output_name" \
        && "$actual_digest" == "$expected_digest" ]] || {
        echo "Stage-0 archive checksum is malformed or incorrect" >&2
        exit 2
    }
fi

root_name="${output_name%.tar.gz}"
if [[ -n "$materialize" ]]; then
    parent="$(dirname "$materialize")"
    [[ -d "$parent" && ! -L "$parent" ]] || {
        echo "Stage-0 materialization requires an existing real parent" >&2
        exit 2
    }
    materialize_lock="${materialize}.publish-lock"
    mkdir "$materialize_lock" 2>/dev/null || {
        echo "another Stage-0 materialization owns the destination" >&2
        exit 2
    }
    [[ ! -e "$materialize" ]] || {
        rmdir "$materialize_lock"
        echo "Stage-0 materialization destination already exists" >&2
        exit 2
    }
    tmp="$(mktemp -d "$parent/.ryeos-stage0-verify.XXXXXX")" || {
        rmdir "$materialize_lock"
        exit 2
    }
else
    tmp="$(mktemp -d)"
fi
cleanup() {
    local status="$1"
    rm -rf "$tmp"
    if [[ -n "$materialize_lock" ]]; then
        rmdir "$materialize_lock"
    fi
    return "$status"
}
trap 'cleanup "$?"' EXIT

names="$tmp/archive-names"
metadata="$tmp/archive-metadata"
listing="$tmp/archive-listing"
tar -tzf "$archive" --quoting-style=literal > "$names"
tar -tvzf "$archive" --numeric-owner --quoting-style=literal > "$listing"
awk '{print $1}' "$listing" > "$metadata"
[[ "$(sed -n '1p' "$names")" == "$root_name/" ]]
[[ "$(wc -l < "$names")" == "$(wc -l < "$metadata")" ]] || {
    echo "Stage-0 archive member metadata is incomplete" >&2
    exit 2
}
[[ "$(wc -l < "$names")" == "$(sort -u "$names" | wc -l)" ]] || {
    echo "Stage-0 archive contains duplicate member names" >&2
    exit 2
}
archive_tree_bytes="$(awk '
    $3 !~ /^[0-9]+$/ { exit 17 }
    { total += $3 }
    END { print total + 0 }
' "$listing")" || {
    echo "Stage-0 archive carries malformed member sizes" >&2
    exit 2
}
archive_entries="$(wc -l < "$names")"
(( archive_tree_bytes <= maximum_tree_bytes \
    && archive_entries <= maximum_tree_entries + 1 )) || {
    echo "Stage-0 archive exceeds its signed entry or expanded-byte bound" >&2
    exit 2
}
while IFS= read -r name || [[ -n "$name" ]]; do
    [[ "$name" == "$root_name/" || "$name" == "$root_name/"* ]]
    [[ ! "$name" =~ [[:cntrl:]] ]]
    [[ "/$name/" != *'/../'* && "/$name/" != *'/./'* && "$name" != *"//"* ]]
done < "$names"
while IFS= read -r mode || [[ -n "$mode" ]]; do
    [[ "$mode" =~ ^[d-][rwx-]{9}$ \
        && "${mode:5:1}" != w \
        && "${mode:8:1}" != w ]] || {
        echo "Stage-0 archive contains a linked, special, or writable entry" >&2
        exit 2
    }
done < "$metadata"

tar -xzf "$archive" --no-same-owner -C "$tmp"
tree="$tmp/$root_name"
[[ -d "$tree" && ! -L "$tree" ]]
[[ "$(find "$tmp" -mindepth 1 -maxdepth 1 -type d -printf '.\n' | awk 'END {print NR + 0}')" -eq 1 ]]

while IFS= read -r -d '' entry; do
    relative="${entry#"$tree/"}"
    [[ ! "$relative" =~ [[:cntrl:]] ]]
    [[ ! -L "$entry" && ( -d "$entry" || -f "$entry" ) ]] || {
        echo "materialized Stage-0 tree contains a linked or special entry" >&2
        exit 2
    }
done < <(find "$tree" -mindepth 1 -print0 | sort -z)

[[ -x "$tree/rust/bin/cargo" && -x "$tree/rust/bin/rustc" \
    && -x "$tree/rust/bin/rustfmt" && -x "$tree/rust/bin/cargo-clippy" \
    && -x "$tree/rust/bin/clippy-driver" && -x "$tree/zig/zig" ]]
[[ -f "$tree/rust/share/doc/rust/COPYRIGHT" \
    && -f "$tree/rust/share/doc/rust/LICENSE-APACHE" \
    && -f "$tree/rust/share/doc/rust/LICENSE-MIT" \
    && -f "$tree/zig/LICENSE" ]] || {
    echo "Stage-0 payload is missing required Rust/Zig license notices" >&2
    exit 2
}
for metadata in components install.log rust-installer-version uninstall.sh \
    manifest-cargo manifest-clippy-preview "manifest-rust-std-$rust_host" \
    manifest-rustc manifest-rustfmt-preview; do
    [[ ! -e "$tree/rust/lib/rustlib/$metadata" ]] || {
        echo "Stage-0 payload contains mutable installer bookkeeping: $metadata" >&2
        exit 2
    }
done
[[ -f "$tree/UPSTREAM-RUST-MANIFEST.toml" \
    && -f "$tree/RYEOS-BOOTSTRAP" \
    && -f "$tree/RYEOS-AUTHORING-PROGRAMS" \
    && -f "$tree/RYEOS-RUNTIME-DEPENDENCIES" \
    && -f "$tree/RYEOS-TREE-SHA256" ]]

retained="$tree/UPSTREAM-RUST-MANIFEST.toml"
[[ "$(stat -c '%s' "$retained")" == "$(contract_value rust_manifest_bytes)" \
    && "$(sha256sum "$retained" | awk '{print $1}')" == "$(contract_value rust_manifest_sha256)" ]]

inputs_sha="$(awk 'NF && $0 !~ /^#/' "$inputs" | sha256sum | awk '{print $1}')"
producer_sha="$(sha256sum "$producer" | awk '{print $1}')"
cat > "$tmp/expected-bootstrap" <<EOF
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
cmp "$tmp/expected-bootstrap" "$tree/RYEOS-BOOTSTRAP" || {
    echo "Stage-0 bootstrap testimony contradicts its source contract" >&2
    exit 2
}

awk '
    NF != 2 { exit 1 }
    $1 !~ /^[0-9a-f]{64}$/ { exit 1 }
    $2 !~ /^\// { exit 1 }
' "$tree/RYEOS-AUTHORING-PROGRAMS" || {
    echo "Stage-0 authoring-program testimony is malformed" >&2
    exit 2
}
[[ -s "$tree/RYEOS-AUTHORING-PROGRAMS" ]]

awk -F '\t' '
    NF != 5 { exit 1 }
    $1 !~ /^[0-9a-f]{64}$/ { exit 1 }
    $2 == "" || $2 ~ /[[:cntrl:]]/ { exit 1 }
    $3 != "elf" && $3 != "non_elf" { exit 1 }
' "$tree/RYEOS-RUNTIME-DEPENDENCIES" || {
    echo "Stage-0 runtime-dependency testimony is malformed" >&2
    exit 2
}
[[ -s "$tree/RYEOS-RUNTIME-DEPENDENCIES" ]]

runtime_verify "$tree" "$tmp"

actual_runtime_dependencies="$tmp/runtime-dependencies"
: > "$actual_runtime_dependencies"
while IFS= read -r -d '' executable; do
    relative="${executable#"$tree/"}"
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
        printf '%s\t%s\telf\t%s\t%s\n' "$executable_sha" "$relative" "${interpreter:--}" "${needed:--}" >> "$actual_runtime_dependencies"
    elif [[ -x "$executable" ]]; then
        executable_sha="$(sha256sum "$executable" | awk '{print $1}')"
        printf '%s\t%s\tnon_elf\t-\t-\n' "$executable_sha" "$relative" >> "$actual_runtime_dependencies"
    fi
done < <(find "$tree/rust" "$tree/zig" "$tree/lib" "$tree/native" -type f -print0 | sort -z)
cmp "$actual_runtime_dependencies" "$tree/RYEOS-RUNTIME-DEPENDENCIES" || {
    echo "Stage-0 executable dependency testimony does not match the payload" >&2
    exit 2
}

actual_tree_manifest="$tmp/tree-sha256"
: > "$actual_tree_manifest"
while IFS= read -r -d '' entry; do
    relative="${entry#"$tree/"}"
    [[ "$relative" != RYEOS-TREE-SHA256 ]] || continue
    mode="$(stat -c '%a' "$entry")"
    if [[ -d "$entry" ]]; then
        printf 'd\t%s\t-\t%s\n' "$mode" "$relative" >> "$actual_tree_manifest"
    else
        printf 'f\t%s\t%s\t%s\n' "$mode" "$(sha256sum "$entry" | awk '{print $1}')" "$relative" >> "$actual_tree_manifest"
    fi
done < <(find "$tree" -mindepth 1 -print0 | sort -z)
cmp "$actual_tree_manifest" "$tree/RYEOS-TREE-SHA256" || {
    echo "Stage-0 retained tree manifest does not match the extracted payload" >&2
    exit 2
}

tree_bytes="$(find "$tree" -type f -printf '%s\n' | awk '{total += $1} END {print total + 0}')"
tree_entries="$(find "$tree" -mindepth 1 -printf '.\n' | awk 'END {print NR + 0}')"
(( tree_bytes <= maximum_tree_bytes && tree_entries <= maximum_tree_entries )) || {
    echo "Stage-0 tree exceeds its signed entry or byte bound" >&2
    exit 2
}

if [[ -n "$materialize" ]]; then
    mv "$tree" "$materialize"
fi
echo "verified exact Stage-0 runtime-closed platform candidate: $archive"
if [[ -n "$materialize" ]]; then
    echo "materialized verified tree: $materialize"
fi
echo "execution remains gated on target-local binding and isolated acceptance"
