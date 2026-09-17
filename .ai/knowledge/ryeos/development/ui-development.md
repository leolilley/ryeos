<!-- ryeos:signed:2026-09-17T01:17:07Z:64af9f017e0f8a5e7a47e0c54bbba028787028982fed5179797e2ad419d1e447:E0J6aj+0fks5YHzoJAv5G1jhM6F9P4y9jYLcOXUhAoLYGpClg1tTAi+3IaqaBmhRb3Goa2VfF8p09jytPdtpBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "ui-development"
title: "Browser UI Development Workflow"
description: "How to develop and qualify the typed Svelte RyeOS browser renderer"
entry_type: reference
version: "2.0.0"
```

# Browser UI Development Workflow

Read `ui-design-system.md` first. Preserve the Gruvbox palette, tiled workspace,
launcher, optional slots, authored ambient character and shared Rust-owned UI
semantics. Do not turn RyeOS into a generic sidebar application.

## Ownership

The browser is a renderer, not a second application model:

- `crates/clients/base/src/ui/` owns semantic state, layout, events and effects.
- `crates/clients/web/src/wasm.rs` is the typed Rust/WASM boundary.
- `crates/clients/web/browser/generated/` is generated from the Rust contract.
- `crates/clients/web/browser/` owns Svelte rendering and browser adapters.
- `crates/clients/web/pkg/` is generated, reviewed installed output. Never edit
  it directly.
- `crates/daemon/ryeos-ui/web-assets.json` is the closed asset manifest.
- `crates/daemon/ryeos-ui/src/assets.rs` serves only that registered closure.

The installed closure is exactly `index.html`, `ryeos_ui.js`, `ryeos_ui.css`,
`ryeos_three.js`, `ryeos_web.js` and `ryeos_web_bg.wasm`. There is no alternate
browser renderer, compatibility bootstrap or runtime fallback.

Svelte owns component lifecycle and presentation. Rust remains the sole owner
of work, authority, navigation, layout and effect semantics. Browser-local
state is limited to mechanical focus, selection, scroll restoration, pending
transport handles and opaque Rust-issued layout preference bytes.

## Focused source loop

Edit authored files under `crates/clients/web/browser/`, then run:

```bash
cd crates/clients/web
npm run check:renderer
npm run build:renderer
```

The build writes a private generation to `target/ui-browser-stage`; it does not
publish `pkg/`. The stage validator rejects extra chunks, source maps, bare
imports, external URLs and any lazy chunk other than `ryeos_three.js`.

Use the local asset proxy or daemon override only with one complete staged or
published generation. Never combine staged JavaScript with embedded WASM from
another generation.

```bash
scripts/dev-ui-assets.sh --background --open
```

The proxy obtains a normal one-shot browser launch from `ryeos web`. Do not
open its `/ui` route directly unless that origin already owns a valid session.

## Required boundary checks

```bash
cargo run -q -p ryeos-ui-contract-exporter -- --check
cd crates/clients/web
npm run check:renderer
npm run build:renderer
npm test
npm run test:wasm-contract
```

The WASM test is not replaceable by a JSON fixture: it proves actual serde /
wasm-bindgen enum, optional and `u64`/BigInt behavior. The mounted-root browser
test additionally requires the exact Playwright browser revision from the
declared development environment; never substitute a host browser silently.

## Ambient scene

`scene_model.rs` emits semantic scene objects. `AmbientLayer.svelte` retains one
`ryeos_ambient_scene.js` controller and updates it from accepted envelopes.
Three.js is the exact local `0.128.0` build chunk; there is no CDN import.
Visibility comes from the authored surface projection, never a browser-side
view-name exception. Animation state does not become RyeOS semantic state.

## Rebuild boundaries

- Svelte/CSS changes require browser stage rebuild and complete asset
  publication.
- Rust UI/WASM changes also require contract export and WASM regeneration.
- Asset registry or route changes require the focused `ryeos-ui` rebuild/tests.
- Bundle definition changes require the normal signed bundle publication path.

Do not invoke Node, npm, Vite or contract generation from Cargo `build.rs`.
Ordinary Cargo builds only embed and validate the checked-in asset generation.

## Common mistakes

- editing `pkg/` instead of authored browser source;
- adding browser-side semantic stores, routing or authority decisions;
- bypassing the single FIFO reducer/effect commit path;
- importing daemon endpoints from visual components;
- restoring predecessor module names or fallback logic;
- using an incomplete asset directory with `RYEOS_UI_ASSET_DIR`;
- accepting a build produced by ambient host tool versions as deterministic
  release evidence.
