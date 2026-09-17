#!/usr/bin/env bash
# ryeos:signed:2026-09-17T01:19:37Z:804e635b24dcc3f4f2207428217152b7de46b776d88ebf2fa74d0da1c1d445fb:+GZrc98YRuWJv7Aydukid9f15HvaJgFPai8NxK/Le423KNz4UsSoSmctCakExgaYxgt2IyhYrXU0nSqD4HYqAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
set -euo pipefail

mode="${1:---check}"
if [[ "$mode" != "--check" && "$mode" != "--publish" ]]; then
  echo "usage: publish.sh [--check|--publish]" >&2
  exit 2
fi

root="$(git rev-parse --show-toplevel)"
web="$root/crates/clients/web"
package="$web/pkg"
registry="$root/crates/daemon/ryeos-ui/generate_asset_registry.py"
stage="$(mktemp -d "$web/.asset-stage.XXXXXX")"
trap 'rm -rf -- "$stage"' EXIT

if [[ "$mode" == "--check" ]]; then
  cargo run -q -p ryeos-ui-contract-exporter -- --check
else
  cargo run -q -p ryeos-ui-contract-exporter
fi

cargo build -q -p ryeos-client-web --target wasm32-unknown-unknown --release --lib
mkdir -p "$stage/wasm" "$stage/renderer" "$stage/final"
wasm-bindgen \
  "$root/target/wasm32-unknown-unknown/release/ryeos_client_web.wasm" \
  --target web \
  --out-dir "$stage/wasm" \
  --out-name ryeos_web

(cd "$web" && RYEOS_UI_ASSET_STAGE="$stage/renderer" npm run check:renderer)
(cd "$web" && RYEOS_UI_ASSET_STAGE="$stage/renderer" npm run build:renderer)
# Vite preserves a small amount of dependency-authored line-end whitespace.
# Normalize generated text before hashing so repository checks and builds from
# different checkout paths compare the same canonical bytes.
sed -i 's/[[:space:]]\+$//' "$stage/renderer"/*.js "$stage/renderer"/*.css

cp "$web/browser/index.html" "$stage/final/index.html"
cp "$stage/renderer/ryeos_ui.js" "$stage/final/ryeos_ui.js"
cp "$stage/renderer/ryeos_ui.css" "$stage/final/ryeos_ui.css"
cp "$stage/renderer/ryeos_three.js" "$stage/final/ryeos_three.js"
cp "$stage/wasm/ryeos_web.js" "$stage/final/ryeos_web.js"
cp "$stage/wasm/ryeos_web_bg.wasm" "$stage/final/ryeos_web_bg.wasm"

expected=$'index.html\nryeos_three.js\nryeos_ui.css\nryeos_ui.js\nryeos_web.js\nryeos_web_bg.wasm'
actual="$(find "$stage/final" -maxdepth 1 -type f -printf '%f\n' | sort)"
if [[ "$actual" != "$expected" ]]; then
  echo "assembled browser asset inventory is not the closed generation" >&2
  exit 1
fi

if [[ "$mode" == "--check" ]]; then
  diff -qr -- "$package" "$stage/final"
  python3 "$registry" --check
  exit 0
fi

backup="$web/.pkg.backup.$$"
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
