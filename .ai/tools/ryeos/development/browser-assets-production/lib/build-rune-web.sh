#!/usr/bin/env bash
# ryeos:signed:2026-11-18T04:15:43Z:c9e7399a104fb0254d0cdf2eff1db5bd09583a51dd1f52f41be54459702cd675:G9zURS3Av9XsHacZlI7LDmBepOb0XVUWcMTGtCOcCrWDx7wB9KTC1R2hfuwkepq23w2Sfx9e1FZ1it8w2nkPCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
# Build the Rune browser renderer into the daemon's closed asset set:
#   crates/clients/rune/pkg/{index.html, ryeos_boot.js, ryeos_rune.js, ryeos_rune_bg.wasm.gz,
#                             ryeos-cjk.ttf.gz, ryeos-emoji.ttf.gz, rune_<snippet>.js ...}
# where the snippets are the ones listed in rune-snippets.toml.
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
# Environment: CARGO (default cargo), CARGO_TARGET_DIR (default <repo>/target),
# WASM_BINDGEN (default wasm-bindgen; must match the locked wasm-bindgen crate).
set -euo pipefail

root=$(cd "$(dirname "$0")/../../../../../.." && pwd)
cargo=${CARGO:-cargo}
target_dir=${CARGO_TARGET_DIR:-$root/target}
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

locked=$(python3 -I -c 'import sys, tomllib; print(next(p["version"] for p in tomllib.load(open(sys.argv[1], "rb"))["package"] if p["name"] == "wasm-bindgen"))' "$root/Cargo.lock")
if [[ $("$bindgen" --version) != "wasm-bindgen $locked" ]]; then
    echo "wasm-bindgen CLI must be $locked (set WASM_BINDGEN): $("$bindgen" --version)" >&2
    exit 1
fi

cargo_home=${CARGO_HOME:-$HOME/.cargo}
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$root=/ryeos --remap-path-prefix=$HOME=/home --remap-path-prefix=$cargo_home=/cargo"
"$cargo" build --locked --profile web -p ryeos-client-rune --target wasm32-unknown-unknown
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
# Stored gzip-encoded: level 9, no file name, no time.
python3 -I - "$dest" "$stage/out/ryeos_rune_bg.wasm" \
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
