#!/usr/bin/env bash
# ryeos:signed:2026-09-17T06:36:51Z:48454233da9c74f51eec0dd41fb53936c86189e9a93ec9c5c465b60624ebb319:uOAfuJh/5Gp26QNgfthWDVocxp9Cv26C85K8jBbNCcnOqGOLlnNBuZxV7hLkiI+QPU3yhcOrP3L2zNBPVfiGBg==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96

# Canonical parser for the deliberately flat Stage0 input Config. Acquisition
# and offline production share this owner. Final artifact verification remains
# independent and deliberately rechecks the signed claims.

stage0_contract_value() {
    local wanted="$1"
    local value
    value="$(awk -v wanted="$wanted" '
        BEGIN { quote=sprintf("%c", 34) }
        $0 ~ "^" wanted ": " quote "[^" quote "]*" quote "$" {
            line=$0
            sub(/^[^:]+: "/, "", line)
            sub(/"$/, "", line)
            print line
            count++
        }
        END { if (count != 1) exit 17 }
    ' "$inputs")" || {
        echo "Stage0 input contract must contain exactly one quoted $wanted field" >&2
        return 2
    }
    printf '%s' "$value"
}

stage0_contract_load() {
    [[ -f "$inputs" && ! -L "$inputs" ]] || {
        echo "Stage0 input contract is missing, linked, or not regular: $inputs" >&2
        return 2
    }

    local allowed_keys
    allowed_keys='category name version schema target source_date_epoch publisher_image rust_version rust_host rust_manifest_url rust_manifest_sha256 rust_manifest_bytes cargo_url cargo_sha256 cargo_bytes clippy_url clippy_sha256 clippy_bytes rust_std_url rust_std_sha256 rust_std_bytes rustc_url rustc_sha256 rustc_bytes rustfmt_url rustfmt_sha256 rustfmt_bytes zig_version zig_url zig_sha256 zig_bytes output_name maximum_output_bytes maximum_tree_bytes maximum_tree_entries execution_gate runtime_mount patchelf_url patchelf_sha256 patchelf_bytes patchelf_program_sha256'

    local line key
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ -z "$line" || "$line" == \#* ]] && continue
        [[ ! "$line" =~ [[:cntrl:]] ]] || {
            echo "Stage0 input contract contains a control character" >&2
            return 2
        }
        [[ "$line" =~ ^([a-z][a-z0-9_]*)\:\ \"[^\"]*\"$ ]] || {
            echo "Stage0 input contract is not a flat quoted-scalar mapping" >&2
            return 2
        }
        key="${BASH_REMATCH[1]}"
        if [[ "$key" =~ ^(image_member|runtime_alias)_[a-z_]+$ ]]; then
            stage0_contract_value "$key" >/dev/null
            continue
        fi
        case " $allowed_keys " in
            *" $key "*) ;;
            *) echo "Stage0 input contract contains unknown field: $key" >&2; return 2 ;;
        esac
    done < "$inputs"
    for key in $allowed_keys; do
        stage0_contract_value "$key" >/dev/null
    done

    schema="$(stage0_contract_value schema)"
    target="$(stage0_contract_value target)"
    epoch="$(stage0_contract_value source_date_epoch)"
    publisher_image="$(stage0_contract_value publisher_image)"
    rust_version="$(stage0_contract_value rust_version)"
    rust_host="$(stage0_contract_value rust_host)"
    zig_version="$(stage0_contract_value zig_version)"
    output_name="$(stage0_contract_value output_name)"
    maximum_output_bytes="$(stage0_contract_value maximum_output_bytes)"
    maximum_tree_bytes="$(stage0_contract_value maximum_tree_bytes)"
    maximum_tree_entries="$(stage0_contract_value maximum_tree_entries)"
    execution_gate="$(stage0_contract_value execution_gate)"

    [[ "$schema" == ryeos.development.stage0-platform-inputs.v3 ]]
    [[ "$(stage0_contract_value category)" == development/ryeos ]]
    [[ "$(stage0_contract_value name)" == stage0-platform-x86_64-linux ]]
    [[ "$(stage0_contract_value version)" == 3.0.0 ]]
    [[ "$target" == x86_64-unknown-linux-gnu && "$rust_host" == "$target" ]]
    [[ "$rust_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
    [[ "$zig_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
    [[ "$epoch" =~ ^[0-9]+$ ]]
    [[ "$publisher_image" =~ ^docker\.io/library/rust@sha256:[0-9a-f]{64}$ ]]
    rust_manifest_url="$(stage0_contract_value rust_manifest_url)"
    rust_dist_base="${rust_manifest_url%/*}"
    [[ "$rust_manifest_url" == https://static.rust-lang.org/dist/*/channel-rust-${rust_version}.toml \
        && "$(stage0_contract_value cargo_url)" == "$rust_dist_base/cargo-${rust_version}-${rust_host}.tar.xz" \
        && "$(stage0_contract_value clippy_url)" == "$rust_dist_base/clippy-${rust_version}-${rust_host}.tar.xz" \
        && "$(stage0_contract_value rust_std_url)" == "$rust_dist_base/rust-std-${rust_version}-${rust_host}.tar.xz" \
        && "$(stage0_contract_value rustc_url)" == "$rust_dist_base/rustc-${rust_version}-${rust_host}.tar.xz" \
        && "$(stage0_contract_value rustfmt_url)" == "$rust_dist_base/rustfmt-${rust_version}-${rust_host}.tar.xz" ]] || {
        echo "Stage0 Rust inputs do not match their exact version/host manifest coordinate" >&2
        return 2
    }
    [[ "$(stage0_contract_value zig_url)" == "https://ziglang.org/download/${zig_version}/zig-x86_64-linux-${zig_version}.tar.xz" ]] || {
        echo "Stage0 Zig input does not match its exact version/target coordinate" >&2
        return 2
    }
    [[ "$maximum_output_bytes" =~ ^[1-9][0-9]*$ ]]
    [[ "$maximum_tree_bytes" =~ ^[1-9][0-9]*$ ]]
    [[ "$maximum_tree_entries" =~ ^[1-9][0-9]*$ ]]
    [[ "$execution_gate" == target_local_binding_and_isolated_acceptance_required ]]

    # Preserve the established Config body identity: comments and blank lines
    # are non-authoritative; every flat scalar row is authoritative.
    input_contract_body_sha256="$(awk 'NF && $0 !~ /^#/' "$inputs" | sha256sum | awk '{print $1}')"
}

# Keep the runtime helper's existing function name while sharing this parser.
contract_value() {
    stage0_contract_value "$@"
}
