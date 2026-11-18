#!/usr/bin/env bash
# ryeos:signed:2026-11-18T07:43:25Z:22c5d9e1e66072c811730a32d7805d4c6b14331f13fc4a4b898c806b17e8eab8:auOhmU7zGAGf4KvsqC0jIc5Q89j7LKcmBAvREMuUpvkTbiIS82DurikhDUCy2U5XbwL8jOqTHMa2gUdMKvSLDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
# Build the Rune browser renderer into the daemon's closed asset set:
#   crates/clients/rune/pkg/{index.html, ryeos_boot.js, ryeos_rune.js, ryeos_rune_bg.wasm.gz,
#                             ryeos-cjk.ttf.gz, ryeos-emoji.ttf.gz, rune_<snippet>.js ...,
#                             <fetched file>.gz ...}
# where the snippets and the files Rune fetches at runtime are the ones listed in
# rune-snippets.toml; the fetched files come from the pinned Rune source.
# then refresh the registry digests. The renderer builds in the size-first `web` profile; it
# and the coverage fonts (crates/clients/rune/web/fonts, from browser-assets-production/lib/build-rune-fonts.py) are stored
# gzip-encoded, which the daemon serves as is. Output is byte-identical across checkouts:
# paths are remapped, no wasm-opt runs, gzip carries no name or time, and wasm-bindgen's
# hashed snippet folders are flattened to fixed names so the registry never sees a rotating
# hash.
#
# --out DIR writes the same files into DIR instead and leaves pkg/ and the registry alone
# (the signed browser-assets-production tool checks and publishes through it).
#
# The renderer is its own Cargo workspace (crates/clients/rune, with its own Cargo.lock), so
# it builds through that manifest and its wasm-bindgen version comes from that lock. Cargo
# hashes a path package outside the building workspace (the shared client crates and
# ryeos-timeline) by its absolute path, and that hash reaches the wasm bytes, so the build
# goes through one fixed path, /tmp/ryeos-rune-web/src, a symlink to this checkout taken
# under a lock. Cargo keeps the symlinked path as written. The path is fixed so the output
# is byte-identical across checkouts and machines. Because it sits in a world-writable
# directory, the script creates /tmp/ryeos-rune-web itself (mode 0700) and refuses to build
# unless it is a real directory, not a symlink, owned by the current user with no group or
# other write. It only ever replaces its own `src` link inside it. Otherwise another local
# user could point the build at their own tree and get it into the signed, checked-in
# package.
#
# Environment: CARGO (default cargo), CARGO_TARGET_DIR (default <repo>/crates/clients/rune/target),
# WASM_BINDGEN (default wasm-bindgen; must match the locked wasm-bindgen crate).
set -euo pipefail

root=$(cd "$(dirname "$0")/../../../../../.." && pwd)
cargo=${CARGO:-cargo}
renderer=$root/crates/clients/rune
target_dir=${CARGO_TARGET_DIR:-$renderer/target}
bindgen=${WASM_BINDGEN:-wasm-bindgen}
pkg=$root/crates/clients/rune/pkg
out=
if [[ $# -gt 0 ]]; then
    [[ $# -eq 2 && $1 == --out ]] || { echo "usage: build-rune-web.sh [--out DIR]" >&2; exit 2; }
    out=$2
    if [[ -e $out && -n $(ls -A "$out") ]]; then
        echo "--out $out is not empty" >&2
        exit 2
    fi
fi

snippet_list=$(dirname "$0")/rune-snippets.toml

locked=$(python3 -I -c 'import sys, tomllib; print(next(p["version"] for p in tomllib.load(open(sys.argv[1], "rb"))["package"] if p["name"] == "wasm-bindgen"))' "$renderer/Cargo.lock")
if [[ $("$bindgen" --version) != "wasm-bindgen $locked" ]]; then
    echo "wasm-bindgen CLI must be $locked (set WASM_BINDGEN): $("$bindgen" --version)" >&2
    exit 1
fi

fixed_parent=/tmp/ryeos-rune-web
fixed=$fixed_parent/src
if [[ ! -e $fixed_parent && ! -L $fixed_parent ]]; then
    mkdir -m 0700 "$fixed_parent" 2>/dev/null || true
fi
# stat without -L reports the entry itself, never a symlink's target.
read -r owner mode kind < <(stat -c '%u %a %F' "$fixed_parent")
if [[ $kind != directory || $owner != "$(id -u)" || $(( 8#$mode & 8#022 )) -ne 0 ]]; then
    echo "$fixed_parent must be a directory owned by $(id -u) with no group or other write (found: $kind, uid $owner, mode $mode)" >&2
    exit 1
fi
exec 9>"$fixed_parent/lock"
flock 9
ln -sfn "$root" "$fixed"

cargo_home=${CARGO_HOME:-$HOME/.cargo}
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/home --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$root=/ryeos --remap-path-prefix=$fixed=/ryeos"
"$cargo" build --locked --profile web --manifest-path "$fixed/crates/clients/rune/Cargo.toml" --target wasm32-unknown-unknown
artifact=$target_dir/wasm32-unknown-unknown/web/ryeos_client_rune.wasm
[[ -f $artifact ]] || { echo "build produced no $artifact" >&2; exit 1; }

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
"$bindgen" --target web --out-dir "$stage/out" --out-name ryeos_rune "$artifact"

# Flatten the snippet modules Rune's web platform ships (exactly those in rune-snippets.toml)
# to rune_<file stem>.js and point the bindings at them.
python3 -I - "$stage/out" "$snippet_list" <<'PY'
import pathlib, re, sys, tomllib
out = pathlib.Path(sys.argv[1])
listed = set(tomllib.loads(pathlib.Path(sys.argv[2]).read_text())["snippets"])
bindings = out / "ryeos_rune.js"
source = bindings.read_text()
specifiers = sorted(set(re.findall(r"""['"](\./snippets/[^'"]+)['"]""", source)))
# A specifier is ./snippets/<crate>-<hash>/<path relative to the crate>.
emitted = {re.sub(r"^\./snippets/[^/]+/", "", s) for s in specifiers}
if emitted != listed or len(emitted) != len(specifiers):
    raise SystemExit(
        "Rune's snippets differ from rune-snippets.toml: "
        f"unexpected={sorted(emitted - listed)} missing={sorted(listed - emitted)} found={specifiers}"
    )
# Only the module specifiers move: the same path string is also the key the wasm module imports
# under, and that key must stay as wasm-bindgen wrote it.
for specifier in specifiers:
    flat = f"rune_{pathlib.PurePosixPath(specifier).stem}.js"
    (out / flat).write_bytes((out / specifier).read_bytes())
    source, count = re.subn(r"""(\bfrom\s*)(['"])""" + re.escape(specifier) + r"\2", rf"\1\2./{flat}\2", source)
    if count == 0:
        raise SystemExit(f"no import of {specifier} to rewrite")
if re.search(r"""\bfrom\s*['"]\./snippets/""", source):
    raise SystemExit("a snippet import survived flattening")
bindings.write_text(source)
PY

dest=${out:-$pkg}
mkdir -p "$dest"
[[ -n $out ]] || find "$pkg" -maxdepth 1 -type f -delete
cp "$stage/out/ryeos_rune.js" "$dest/"
python3 -I - "$snippet_list" <<'PY' | while read -r flat; do cp "$stage/out/$flat" "$dest/"; done
import pathlib, sys, tomllib
for path in tomllib.loads(pathlib.Path(sys.argv[1]).read_text())["snippets"]:
    print(f"rune_{pathlib.PurePosixPath(path).stem}.js")
PY
cp "$root/crates/clients/rune/web/index.html" "$root/crates/clients/rune/web/ryeos_boot.js" "$dest/"
# The files Rune fetches at runtime (rune-snippets.toml `fetched`), from the pinned Rune source.
fetched_list=$(CARGO=$cargo python3 -I "$(dirname "$0")/check-rune-snippets.py" --fetched-paths)
mapfile -t fetched < <(printf '%s' "$fetched_list")
# Stored gzip-encoded: level 9, no file name, no time.
python3 -I - "$dest" "$stage/out/ryeos_rune_bg.wasm" "${fetched[@]}" \
    "$root/crates/clients/rune/web/fonts/ryeos-cjk.ttf" "$root/crates/clients/rune/web/fonts/ryeos-emoji.ttf" <<'PY'
import gzip, pathlib, sys
dest = pathlib.Path(sys.argv[1])
for source in map(pathlib.Path, sys.argv[2:]):
    data = source.read_bytes()
    packed = gzip.compress(data, compresslevel=9, mtime=0)
    (dest / f"{source.name}.gz").write_bytes(packed)
    print(f"{source.name}: {len(data):,} bytes, {len(packed):,} gzip")
PY

if [[ -z $out ]]; then
    python3 -I "$root/crates/daemon/ryeos-ui/generate_asset_registry.py" --refresh-digests
fi
