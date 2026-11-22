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
# Comments (`//` and nested `/* */`) are stripped before matching, outside
# string and char literals only (scripts/ci/strip-rust-comments.py), so a
# `//` inside a URL string hides nothing. Also banned: `union Name` declarations,
# so a union pun cannot stand in for a banned cast.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

STRIP="$ROOT/scripts/ci/strip-rust-comments.py"

# The one accepted foreign-function handoff: a lease boxed for SQLite's C API.
EXCEPTION_FILE=crates/kernel/lillux-sqlite/src/binding.rs
EXCEPTION_PATTERN='Box::into_raw[[:space:]]*\('

# The one accepted function-pointer retyping, in a lillux-sqlite test: SQLite's
# `sqlite3_syscall_ptr` is a type-erased fn pointer, so no typed cast exists.
TRANSMUTE_EXCEPTION_FILE=crates/kernel/lillux-sqlite/src/lib.rs
TRANSMUTE_EXCEPTION_PATTERN='transmute::<'

banned='(^|[^A-Za-z0-9_])forget[[:space:]]*(\(|::<)|mem::forget|mem::\{[^}]*forget([^A-Za-z0-9_]|$)'
banned+='|ManuallyDrop|MaybeUninit|mem::zeroed|mem::\{[^}]*zeroed|ptr::read|ptr::\{[^}]*read'
banned+='|Box::leak|::leak[[:space:]]*\(|\.leak[[:space:]]*\([[:space:]]*\)|::into_raw[[:space:]]*\(|\.into_raw[[:space:]]*\('
banned+='|(std|core)::mem[[:space:]]+as[[:space:]]|(std|core)::mem::[A-Za-z_]+[[:space:]]+as[[:space:]]'
banned+='|(std|core)::mem::\{[^}]*[[:space:]]as[[:space:]]|(std|core)::\{[^}]*mem[[:space:]]+as[[:space:]]'
banned+='|mem::transmute|mem::\{[^}]*transmute|ptr::drop_in_place|ptr::\{[^}]*drop_in_place'
banned+='|(^|[^A-Za-z0-9_])(transmute|drop_in_place)[[:space:]]*(\(|::<)'
banned+='|(^|[^A-Za-z0-9_.])union[[:space:]]+[A-Z][A-Za-z0-9_]*[[:space:]]*[<{]'
lint_silencer='#!?\[[[:space:]]*(allow|expect)[[:space:]]*\(|#!?\[[[:space:]]*cfg_attr[[:space:]]*\(.*(allow|expect)[[:space:]]*\('

# Print every banned line of one file as file:line: text.
# The optional second argument names the path used for the exceptions.
scan() {
    local file="$1" label="${2:-$1}" pattern="$banned"
    if grep -qE 'lillux::custody|Held<' "$file"; then
        pattern+="|$lint_silencer"
    fi
    python3 "$STRIP" <"$file" | grep -nE "$pattern" | while IFS= read -r hit; do
        if [[ "$label" == "$EXCEPTION_FILE" ]] && grep -qE "$EXCEPTION_PATTERN" <<<"${hit#*:}"; then
            continue
        fi
        if [[ "$label" == "$TRANSMUTE_EXCEPTION_FILE" ]] && grep -qE "$TRANSMUTE_EXCEPTION_PATTERN" <<<"${hit#*:}"; then
            continue
        fi
        printf '%s:%s\n' "$file" "$hit"
    done || true
}

# --self-test: prove the comment stripper hides only real comments.
if [[ "${1:-}" == --self-test ]]; then
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' EXIT
    check() { # name, expected hit count, source, optional path for exceptions
        printf '%s\n' "$3" >"$dir/case.rs"
        local got
        got="$(scan "$dir/case.rs" "${4:-}" | wc -l)"
        if [[ "$got" != "$2" ]]; then
            printf 'self-test %s: expected %s hits, got %s\n' "$1" "$2" "$got" >&2
            exit 1
        fi
    }
    check url-then-token 1 'let u = "http://x"; std::mem::forget(o);'
    check line-comment 0 'let a = 1; // std::mem::forget(o)'
    check block-comment 0 '/* std::mem::forget(o) */ let a = 1;'
    check multiline-block 0 $'/* open\n std::mem::forget(o)\n*/'
    check nested-block 0 '/* a /* b */ std::mem::forget(o) */'
    check code-after-block 1 '/* c */ std::mem::forget(o);'
    check raw-string-url 1 'let u = r#"http://x"#; ManuallyDrop::new(o);'
    check method-into-raw 1 'let p = cstring.into_raw();'
    check path-into-raw 1 'let p = Box::into_raw(b);'
    check lifetime-not-char 1 "fn f<'a>(x: &'a str) { ManuallyDrop::new(x); }"
    check transmute 1 'let f = std::mem::transmute::<A, B>(g);'
    check drop-in-place 1 'unsafe { p.drop_in_place(); }'
    check allow-in-custody-file 1 $'use lillux::custody::Held;\n#[allow(dead_code)]'
    check expect-in-custody-file 1 $'let h: Held<u8>;\n#[expect(unused)]'
    check cfg-attr-allow 1 $'use lillux::custody::Held;\n#[cfg_attr(test, allow(dead_code))]'
    check allow-outside-custody-file 0 '#[allow(dead_code)]'
    check union-decl 1 'union Cast { a: u8, b: i8 }'
    check union-method 0 'let u = a.union(&b);'
    check into-raw-exception 0 'let p = Box::into_raw(b);' crates/kernel/lillux-sqlite/src/binding.rs
    check into-raw-elsewhere 1 'let p = Box::into_raw(b);' crates/x/src/lib.rs
    check transmute-exception 0 'let f = std::mem::transmute::<A, B>(g);' crates/kernel/lillux-sqlite/src/lib.rs
    check transmute-exception-elsewhere 1 'let f = std::mem::transmute::<A, B>(g);' crates/kernel/lillux-sqlite/src/native.rs
    check transmute-exception-other-token 1 'let f = std::mem::transmute_copy(&g);' crates/kernel/lillux-sqlite/src/lib.rs
    printf 'self-test passed\n'
    exit 0
fi

status=0
while IFS= read -r file; do
    [[ -f "$file" ]] || continue
    hits="$(scan "$file")"
    [[ -n "$hits" ]] || continue
    printf '%s\n' "$hits"
    status=1
done < <(git ls-files -co --exclude-standard -- 'crates/*.rs' 'tests/*.rs' \
    | grep -v '^crates/kernel/lillux/src/' | sort -u)

if [[ "$status" != 0 ]]; then
    printf 'custody token ban refused (see lines above)\n' >&2
fi
exit "$status"
