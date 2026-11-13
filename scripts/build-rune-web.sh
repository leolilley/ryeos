#!/usr/bin/env bash
# Build the Rune browser renderer into the daemon's closed asset set:
#   crates/clients/rune/pkg/{index.html, ryeos_boot.js, ryeos_rune.js, rune_bridge.js,
#                             rune_platform.js, ryeos_rune_bg.wasm.gz, ryeos-cjk.ttf.gz,
#                             ryeos-emoji.ttf.gz}
# then refresh the registry digests. The renderer builds in the size-first `web` profile; it
# and the coverage fonts (crates/clients/rune/web/fonts, from build-rune-fonts.py) are stored
# gzip-encoded, which the daemon serves as is. Output is byte-identical across checkouts:
# paths are remapped, no wasm-opt runs, gzip carries no name or time, and wasm-bindgen's
# hashed snippet folders are flattened to fixed names so the registry never sees a rotating
# hash.
#
# --out DIR writes the eight files into DIR instead and leaves pkg/ and the registry alone
# (the signed browser-assets-production tool checks and publishes through it).
#
# Environment: CARGO (default cargo), CARGO_TARGET_DIR (default <repo>/target),
# WASM_BINDGEN (default wasm-bindgen; must match the locked wasm-bindgen crate).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
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

# Flatten the two snippet modules Rune's web platform ships and point the bindings at them.
python3 -I - "$stage/out" <<'PY'
import pathlib, re, sys
out = pathlib.Path(sys.argv[1])
bindings = out / "ryeos_rune.js"
source = bindings.read_text()
specifiers = sorted(set(re.findall(r"""['"](\./snippets/[^'"]+)['"]""", source)))
names = {"bridge.js": "rune_bridge.js", "platform.js": "rune_platform.js"}
if len(specifiers) != 2 or {pathlib.PurePosixPath(s).name for s in specifiers} != set(names):
    raise SystemExit(f"expected exactly Rune's bridge and platform snippets, found {specifiers}")
# Only the module specifiers move: the same path string is also the key the wasm module imports
# under, and that key must stay as wasm-bindgen wrote it.
for specifier in specifiers:
    flat = names[pathlib.PurePosixPath(specifier).name]
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
cp "$stage/out/ryeos_rune.js" "$stage/out/rune_bridge.js" "$stage/out/rune_platform.js" "$dest/"
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
