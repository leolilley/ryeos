#!/usr/bin/env bash

# Canonical shared Stage-0 publisher/verifier functions. Source ownership here
# does not grant execution: the external first-bootstrap publisher and the
# later admitted producer must call this same implementation.
# The signed input owns all image members. These functions run only during
# artifact authoring/verification; RyeOS import and binding still own launch.
# Do not replace this with execution-host discovery, compiler wrappers, or a
# second manifest/environment authority. Stage 1 must reproduce this tree via
# ordinary admitted project Tools before retiring the bootstrap procedure.

runtime_member_path() {
    [[ "$1" =~ ^[a-zA-Z0-9_+.-]+(/[a-zA-Z0-9_+.-]+)*$ \
        && "/$1/" != *'/../'* && "/$1/" != *'/./'* ]]
}

runtime_contract_validate() {
    local key source destination mode bytes sha extra
    runtime_mount="$(contract_value runtime_mount)"
    [[ "$runtime_mount" == /ryeos/realizations/platform ]]
    runtime_loader="$runtime_mount/lib/ld-linux-x86-64.so.2"
    runtime_runpath="$runtime_mount/lib:$runtime_mount/rust/lib:$runtime_mount/rust/lib/rustlib/$rust_host/lib"
    [[ "$(contract_value patchelf_url)" == https://deb.debian.org/debian/pool/main/p/patchelf/patchelf_0.18.0-1.4_amd64.deb ]]
    [[ "$(contract_value patchelf_sha256)" =~ ^[0-9a-f]{64}$ \
        && "$(contract_value patchelf_program_sha256)" =~ ^[0-9a-f]{64}$ \
        && "$(contract_value patchelf_bytes)" =~ ^[1-9][0-9]*$ ]]
    mapfile -t image_member_keys < <(sed -n 's/^\(image_member_[a-z_]*\): .*/\1/p' "$inputs" | sort)
    mapfile -t runtime_alias_keys < <(sed -n 's/^\(runtime_alias_[a-z_]*\): .*/\1/p' "$inputs" | sort)
    (( ${#image_member_keys[@]} > 0 && ${#image_member_keys[@]} <= 64 \
        && ${#runtime_alias_keys[@]} > 0 && ${#runtime_alias_keys[@]} <= 32 ))
    local -A destinations=()
    for key in "${image_member_keys[@]}"; do
        read -r source destination mode bytes sha extra <<< "$(contract_value "$key")"
        [[ -z "$extra" && "$source" == /usr/* ]]
        runtime_member_path "${source#/}"
        runtime_member_path "$destination"
        [[ "$destination" == lib/* || "$destination" == native/bin/* || "$destination" == share/licenses/* ]]
        [[ "$mode" == 644 || "$mode" == 755 ]]
        [[ "$bytes" =~ ^[1-9][0-9]*$ && "$sha" =~ ^[0-9a-f]{64}$ ]]
        [[ ! -v "destinations[$destination]" ]] || {
            echo "duplicate runtime destination: $destination" >&2; return 2;
        }
        destinations[$destination]=1
    done
    for key in "${runtime_alias_keys[@]}"; do
        read -r source destination extra <<< "$(contract_value "$key")"
        [[ -z "$extra" ]]
        runtime_member_path "$source"
        runtime_member_path "$destination"
        [[ "$destination" == lib/* || "$destination" == native/bin/* ]]
        [[ ! -v "destinations[$destination]" ]] || {
            echo "duplicate runtime destination: $destination" >&2; return 2;
        }
        destinations[$destination]=1
    done
}

runtime_symbol_snapshot() {
    local member="$1" output="$2"
    readelf -W --section-headers "$member" \
        | sed -n 's/^ *\[ *\([0-9]*\)\] *\([^ ]*\) .*/\1 \2/p' > "$output.sections"
    readelf -W --symbols "$member" | awk '
        NR==FNR {section[$1]=$2; next}
        $1 ~ /^[0-9]+:$/ {
            idx=$7
            if (idx ~ /^[0-9]+$/) {
                if (!(idx in section)) exit 1
                idx=section[idx]
            }
            print $4,$5,$6,idx,$8
        }
    ' "$output.sections" - | sort > "$output.symbols"
    readelf -W --symbols "$member" | awk '
        $1 ~ /^[0-9]+:$/ && $4 == "FUNC" {print $2,$3,$4,$5,$6,$8}
    ' | sort > "$output.functions"
}

runtime_assemble() {
    local tree="$1" scratch="$2" package="$3"
    local key source destination mode bytes sha extra member relative magic before after
    local patcher="$scratch/patchelf"
    ar p "$package" data.tar.xz | tar -xOJf - ./usr/bin/patchelf > "$patcher"
    [[ "$(sha256sum "$patcher" | awk '{print $1}')" == "$(contract_value patchelf_program_sha256)" ]]
    chmod 0755 "$patcher"
    mkdir -p "$tree/lib" "$tree/native/bin" "$tree/sysroot"
    : > "$tree/RYEOS-RUNTIME-SOURCES"
    for key in "${image_member_keys[@]}"; do
        read -r source destination mode bytes sha extra <<< "$(contract_value "$key")"
        # Canonical member paths must not silently traverse image aliases.
        [[ -f "$source" && ! -L "$source" && "$(readlink -f "$source")" == "$source" \
            && "$(stat -c '%s' "$source")" == "$bytes" \
            && "$(stat -c '%a' "$source")" == "$mode" \
            && "$(sha256sum "$source" | awk '{print $1}')" == "$sha" ]] || {
            echo "pinned publisher member contradicts $key" >&2; return 2;
        }
        [[ ! -e "$tree/$destination" ]]
        install -D -m "$mode" "$source" "$tree/$destination"
        printf '%s\t%s\t%s\t%s\t%s\n' "$source" "$destination" "$mode" "$bytes" "$sha" >> "$tree/RYEOS-RUNTIME-SOURCES"
    done
    for key in "${runtime_alias_keys[@]}"; do
        read -r source destination extra <<< "$(contract_value "$key")"
        [[ -f "$tree/$source" && ! -L "$tree/$source" && ! -e "$tree/$destination" ]]
        cp "$tree/$source" "$tree/$destination"
    done
    # These upstream launchers require undeclared Python/GDB/LLDB programs.
    # They are outside the finite Cargo/rustc/rustfmt/Clippy operation set.
    for relative in rust/bin/rust-gdb rust/bin/rust-gdbgui rust/bin/rust-lldb; do
        [[ -f "$tree/$relative" && ! -L "$tree/$relative" ]]
        rm -- "$tree/$relative"
    done
    : > "$tree/RYEOS-ELF-TRANSFORMS"
    while IFS= read -r -d '' member; do
        magic=""
        IFS= read -r -n 4 -d '' magic < "$member" || true
        [[ "$magic" == $'\177ELF' ]] || continue
        readelf -h "$member" | grep -E 'Type:.*(DYN|EXEC)' >/dev/null || continue
        # Static executables and relocatable objects have no loader edges.
        readelf -d "$member" | grep 'Dynamic section' >/dev/null || continue
        relative="${member#"$tree/"}"
        [[ "$relative" != lib/ld-linux-x86-64.so.2 ]] || continue
        before="$(sha256sum "$member" | awk '{print $1}')"
        runtime_symbol_snapshot "$member" "$scratch/before"
        if readelf -l "$member" | grep 'Requesting program interpreter:' >/dev/null; then
            "$patcher" --no-sort --set-interpreter "$runtime_loader" "$member"
        fi
        # The selected upstream tool's section sorting changed libgcc symbol
        # ownership in qualification. Preserve section order explicitly and
        # still prove symbol/section equivalence; never waive the check.
        "$patcher" --no-sort --set-rpath "$runtime_runpath" --no-default-lib "$member"
        runtime_symbol_snapshot "$member" "$scratch/after"
        # A successful smoke test cannot excuse a malformed ELF rewrite.
        # Compare section ownership and function addresses, including stripped
        # dynamic symbols, for every transformed file rather than one library.
        cmp "$scratch/before.symbols" "$scratch/after.symbols" || {
            echo "ELF symbol ownership changed: $relative" >&2
            return 2
        }
        cmp "$scratch/before.functions" "$scratch/after.functions" || {
            echo "ELF function coordinates changed: $relative" >&2; return 2;
        }
        after="$(sha256sum "$member" | awk '{print $1}')"
        printf '%s\t%s\t%s\n' "$relative" "$before" "$after" >> "$tree/RYEOS-ELF-TRANSFORMS"
    done < <(find "$tree/rust" "$tree/zig" "$tree/lib" "$tree/native" -type f -print0 | sort -z)
    runtime_verify "$tree" "$scratch"
}

runtime_verify() {
    local tree="$1" scratch="$2"
    local key source destination mode bytes sha extra member relative magic interpreter dynamic needed found directory before after
    [[ -f "$tree/RYEOS-RUNTIME-SOURCES" && -f "$tree/RYEOS-ELF-TRANSFORMS" \
        && -d "$tree/sysroot" && -z "$(find "$tree/sysroot" -mindepth 1 -print -quit)" ]]
    : > "$scratch/runtime-sources.expected"
    local -A transformed=() originals=()
    while IFS=$'\t' read -r relative before after extra; do
        [[ -z "$extra" && "$before" =~ ^[0-9a-f]{64}$ && "$after" =~ ^[0-9a-f]{64}$ ]]
        runtime_member_path "$relative"
        [[ ! -v "transformed[$relative]" && -f "$tree/$relative" \
            && "$(sha256sum "$tree/$relative" | awk '{print $1}')" == "$after" ]]
        transformed[$relative]=1
        originals[$relative]="$before"
    done < "$tree/RYEOS-ELF-TRANSFORMS"
    for key in "${image_member_keys[@]}"; do
        read -r source destination mode bytes sha extra <<< "$(contract_value "$key")"
        printf '%s\t%s\t%s\t%s\t%s\n' "$source" "$destination" "$mode" "$bytes" "$sha" >> "$scratch/runtime-sources.expected"
        [[ -f "$tree/$destination" && ! -L "$tree/$destination" \
            && "$(stat -c '%a' "$tree/$destination")" == "$mode" ]]
        if [[ -v "originals[$destination]" ]]; then
            [[ "${originals[$destination]}" == "$sha" ]]
        else
            [[ "$(stat -c '%s' "$tree/$destination")" == "$bytes" \
                && "$(sha256sum "$tree/$destination" | awk '{print $1}')" == "$sha" ]]
        fi
    done
    cmp "$scratch/runtime-sources.expected" "$tree/RYEOS-RUNTIME-SOURCES"
    for key in "${runtime_alias_keys[@]}"; do
        read -r source destination extra <<< "$(contract_value "$key")"
        cmp "$tree/$source" "$tree/$destination"
    done
    for relative in rust/bin/rust-gdb rust/bin/rust-gdbgui rust/bin/rust-lldb; do
        [[ ! -e "$tree/$relative" ]]
    done
    while IFS= read -r -d '' member; do
        relative="${member#"$tree/"}"
        magic=""
        IFS= read -r -n 4 -d '' magic < "$member" || true
        if [[ "$magic" != $'\177ELF' ]]; then
            [[ ! -x "$member" ]] || {
                echo "undeclared non-ELF launcher: $relative" >&2; return 2;
            }
            continue
        fi
        readelf -h "$member" >/dev/null
        dynamic="$(readelf -d "$member")"
        interpreter="$(readelf -l "$member" | sed -n 's/.*Requesting program interpreter: \([^]]*\)].*/\1/p')"
        [[ -z "$interpreter" || "$interpreter" == "$runtime_loader" ]] || {
            echo "unclosed ELF interpreter: $relative" >&2; return 2;
        }
        if [[ "$dynamic" == *'Dynamic section'* && "$relative" != lib/ld-linux-x86-64.so.2 ]]; then
            [[ -v "transformed[$relative]" ]]
            [[ "$(sed -n 's/.*Library runpath: \[\([^]]*\)\].*/\1/p' <<< "$dynamic")" == "$runtime_runpath" \
                && "$dynamic" == *NODEFLIB* && "$dynamic" != *'(RPATH)'* ]] || {
                echo "unclosed ELF search authority: $relative" >&2; return 2;
            }
            unset 'transformed[$relative]'
        fi
        while IFS= read -r needed; do
            [[ "$needed" =~ ^[a-zA-Z0-9_+.-]+$ ]]
            found=0
            for directory in lib rust/lib "rust/lib/rustlib/$rust_host/lib"; do
                if [[ -f "$tree/$directory/$needed" && ! -L "$tree/$directory/$needed" ]]; then
                    magic=""
                    IFS= read -r -n 4 -d '' magic < "$tree/$directory/$needed" || true
                    [[ "$magic" == $'\177ELF' ]] || {
                        echo "ELF dependency resolves to non-ELF data: $needed" >&2; return 2;
                    }
                    found=1
                    break
                fi
            done
            [[ "$found" == 1 ]] || {
                echo "missing recursive ELF dependency: $relative -> $needed" >&2; return 2;
            }
        done < <(sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p' <<< "$dynamic")
    done < <(find "$tree/rust" "$tree/zig" "$tree/lib" "$tree/native" -type f -print0 | sort -z)
    (( ${#transformed[@]} == 0 ))
}
