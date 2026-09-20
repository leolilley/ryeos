#!/usr/bin/env bash
# ryeos:signed:2026-09-19T01:31:04Z:ba1d2160a7ed75b7ffd95c326bef24f497ed825cd594a695f76f6d4e0b843296:sRVHWotKEkua3XfU65zykqC8HSO0RUYaZ7xBRMJbIR7yxhNISwZ+Fc9Nhm04Q52u/HMj1AovgWZPbmQq1MqFAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
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

expected=(
  index.html
  ryeos_three.js
  ryeos_ui.css
  ryeos_ui.js
  ryeos_web.js
  ryeos_web_bg.wasm
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
