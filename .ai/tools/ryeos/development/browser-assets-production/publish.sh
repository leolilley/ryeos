#!/usr/bin/env bash
# ryeos:signed:2026-11-18T04:15:43Z:a65ff5359646e8b653a097ca3aa938bcf25963138c58a81e32625e7524f5d9d9:INXBgV120DT9tiT4xvKkGrx71FR9Ie9k28wfLVg8F/i3zAPD396bjwMFEk86jMKvdYRloWbmeOk7QwmG3Mp4Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
set -euo pipefail

mode="${1:---check}"
if [[ "$mode" != "--check" && "$mode" != "--publish" ]]; then
  echo "usage: publish.sh [--check|--publish]" >&2
  exit 2
fi

root="$(git rev-parse --show-toplevel)"
renderer="$root/crates/clients/rune"
package="$renderer/pkg"
registry="$root/crates/daemon/ryeos-ui/generate_asset_registry.py"
stage="$(mktemp -d "$renderer/.asset-stage.XXXXXX")"
trap 'rm -rf -- "$stage"' EXIT

# The Rune renderer's closed set: built with remapped paths and the exact
# wasm-bindgen CLI the lock names (the build refuses any other version).
"$root/.ai/tools/ryeos/development/browser-assets-production/lib/build-rune-web.sh" --out "$stage/final"

# The closed generation is whatever the registry lists; the registry check below
# holds it to rune-snippets.toml and to the package.
mapfile -t expected < <(python3 -I -c 'import json, sys; print("\n".join(sorted(a["filename"] for a in json.load(open(sys.argv[1]))["assets"])))' "$root/crates/daemon/ryeos-ui/web-assets.json" | LC_ALL=C sort)
mapfile -t actual < <(find "$stage/final" -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort)
if [[ "${actual[*]}" != "${expected[*]}" ]]; then
  echo "assembled browser asset inventory is not the closed generation" >&2
  printf 'expected: %s\n' "${expected[*]}" >&2
  printf 'actual:   %s\n' "${actual[*]}" >&2
  exit 1
fi

if [[ "$mode" == "--check" ]]; then
  diff -qr -- "$package" "$stage/final"
  python3 "$registry" --check
  exit 0
fi

backup="$renderer/.pkg.backup.$$"
manifest="$root/crates/daemon/ryeos-ui/web-assets.json"
generated="$root/crates/daemon/ryeos-ui/src/generated_web_assets.rs"
cp "$manifest" "$stage/web-assets.json"
cp "$generated" "$stage/generated_web_assets.rs"
mv "$package" "$backup"
if ! mv "$stage/final" "$package"; then
  mv "$backup" "$package"
  exit 1
fi
if ! python3 "$registry" --refresh-digests; then
  rm -rf -- "$package"
  mv "$backup" "$package"
  cp "$stage/web-assets.json" "$manifest"
  cp "$stage/generated_web_assets.rs" "$generated"
  exit 1
fi
rm -rf -- "$backup"
python3 "$registry" --check
