#!/usr/bin/env bash
# ryeos:signed:2026-11-11T22:32:15Z:c1412beb510cbd5be833408a3d139c5655ef61f76030204f21d65e46bcb6ef60:Q92b+ilDjjxp087WioqJGRs2hmLkxSl+Q5OJiu37032jrdfk26s+v4mglJmy/f3k3ntjH+GbQmWQHtN5j+qTBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
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

if [[ "$mode" == "--check" ]]; then
  cargo run -q -p ryeos-ui-contract-exporter -- --check
else
  cargo run -q -p ryeos-ui-contract-exporter
fi

# The Rune renderer's closed set: built with remapped paths and the exact
# wasm-bindgen CLI the lock names (the build refuses any other version).
"$root/scripts/build-rune-web.sh" --out "$stage/final"

expected=(
  index.html
  rune_bridge.js
  rune_platform.js
  ryeos_boot.js
  ryeos_rune.js
  ryeos_rune_bg.wasm
)
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
