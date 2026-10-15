#!/usr/bin/env bash
# Refuse the tokens that release or skip an owner's destructor outside lillux.
#
# Custody of native handles, descriptors and locks is supervised through
# lillux::custody (Held, abandon, relinquish). Anything that ends an owner
# without its destructor (forget, ManuallyDrop, MaybeUninit, mem::zeroed,
# ptr::read, leaking, into_raw, a renamed std::mem/core::mem import) belongs
# only in crates/kernel/lillux/src. A file that mentions lillux::custody or
# Held< may not silence lints with #[allow(..)] or #[expect(..)] either.
#
# Also banned outside lillux: mem::transmute and ptr::drop_in_place.
#
# `//` comments are stripped before matching. Files on the LEGACY list are not
# yet migrated; each carries the maximum number of banned lines it may still
# hold (file:max). More hits than that fail, so a listed file cannot gain new
# ones; once a listed file has no hits left it must be removed from the list,
# so the list only shrinks.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# The one accepted foreign-function handoff: a lease boxed for SQLite's C API.
EXCEPTION_FILE=crates/kernel/lillux-sqlite/src/binding.rs
EXCEPTION_PATTERN='Box::into_raw[[:space:]]*\('

LEGACY=(
    crates/bin/daemon/src/main.rs:7
    crates/daemon/ryeos-node/src/native_capacity/reservation.rs:6
    crates/kernel/lillux-sqlite/src/binding.rs:1
    crates/kernel/lillux-sqlite/src/custody_drop_gate.rs:18
    crates/kernel/lillux-sqlite/src/lib.rs:19
    crates/kernel/lillux-sqlite/src/native.rs:134
    crates/kernel/lillux-sqlite/src/original_host_owner.rs:13
    crates/kernel/lillux-sqlite/src/original_physical_custody.rs:9
    crates/state/ryeos-state/src/custody_drop_gate.rs:46
)

banned='(^|[^A-Za-z0-9_])forget[[:space:]]*(\(|::<)|mem::forget|mem::\{[^}]*forget([^A-Za-z0-9_]|$)'
banned+='|ManuallyDrop|MaybeUninit|mem::zeroed|mem::\{[^}]*zeroed|ptr::read|ptr::\{[^}]*read'
banned+='|Box::leak|::leak[[:space:]]*\(|\.leak[[:space:]]*\([[:space:]]*\)|::into_raw[[:space:]]*\('
banned+='|(std|core)::mem[[:space:]]+as[[:space:]]|(std|core)::mem::[A-Za-z_]+[[:space:]]+as[[:space:]]'
banned+='|(std|core)::mem::\{[^}]*[[:space:]]as[[:space:]]|(std|core)::\{[^}]*mem[[:space:]]+as[[:space:]]'
banned+='|mem::transmute|mem::\{[^}]*transmute|ptr::drop_in_place|ptr::\{[^}]*drop_in_place'
banned+='|(^|[^A-Za-z0-9_])(transmute|drop_in_place)[[:space:]]*(\(|::<)'
lint_silencer='#!?\[[[:space:]]*(allow|expect)[[:space:]]*\(|#!?\[[[:space:]]*cfg_attr[[:space:]]*\(.*(allow|expect)[[:space:]]*\('

# Print every banned line of one file as file:line: text.
scan() {
    local file="$1" pattern="$banned"
    if grep -qE 'lillux::custody|Held<' "$file"; then
        pattern+="|$lint_silencer"
    fi
    sed 's://.*$::' "$file" | grep -nE "$pattern" | while IFS= read -r hit; do
        if [[ "$file" == "$EXCEPTION_FILE" ]] && grep -qE "$EXCEPTION_PATTERN" <<<"${hit#*:}"; then
            continue
        fi
        printf '%s:%s\n' "$file" "$hit"
    done || true
}

declare -A legacy=() maximum=()
for entry in "${LEGACY[@]}"; do
    legacy["${entry%:*}"]=0
    maximum["${entry%:*}"]="${entry##*:}"
done

status=0
while IFS= read -r file; do
    [[ -f "$file" ]] || continue
    hits="$(scan "$file")"
    [[ -n "$hits" ]] || continue
    if [[ -v "legacy[$file]" ]]; then
        count="$(wc -l <<<"$hits")"
        legacy["$file"]="$count"
        if (( count > maximum["$file"] )); then
            printf '%s\n' "$hits"
            printf '%s: %s banned lines exceed the LEGACY maximum of %s\n' \
                "$file" "$count" "${maximum[$file]}"
            status=1
        fi
        continue
    fi
    printf '%s\n' "$hits"
    status=1
done < <(git ls-files -co --exclude-standard -- 'crates/*.rs' 'tests/*.rs' \
    | grep -v '^crates/kernel/lillux/src/' | sort -u)

for entry in "${LEGACY[@]}"; do
    file="${entry%:*}"
    if [[ "${legacy[$file]}" == 0 ]]; then
        printf '%s: no banned tokens remain; remove it from LEGACY\n' "$file"
        status=1
    fi
done

if [[ "$status" != 0 ]]; then
    printf 'custody token ban refused (see lines above)\n' >&2
fi
exit "$status"
