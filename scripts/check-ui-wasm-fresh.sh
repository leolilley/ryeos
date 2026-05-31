#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build -p ryeos-ui-web --target wasm32-unknown-unknown --lib
wasm-bindgen \
  target/wasm32-unknown-unknown/debug/ryeos_ui_web.wasm \
  --target web \
  --out-dir crates/clients/web/pkg \
  --out-name ryeos_web
rm -f crates/clients/web/pkg/ryeos_web.d.ts crates/clients/web/pkg/ryeos_web_bg.wasm.d.ts

if ! git diff --quiet -- crates/clients/web/pkg/ryeos_web.js crates/clients/web/pkg/ryeos_web_bg.wasm; then
  echo "UI WASM generated assets changed. Review and commit crates/clients/web/pkg/ryeos_web*." >&2
  exit 1
fi
