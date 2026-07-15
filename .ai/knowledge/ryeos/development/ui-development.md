<!-- ryeos:signed:2026-07-13T07:43:47Z:d3d43d7b3a43de79215c1669a750105706c53480ab3370e34b4d7eac75cccde9:9Ql/8hkc708X6AJsEzfpTlEtJ913/Da9TyhionSGdkUumJakALqzjkzyZTwEAXOH1b0AGKSH515z8qqFxPCmAg==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
```yaml
category: "ryeos/development"
name: "ui-development"
title: "Browser UI Development Workflow"
description: "How to iterate on RyeOS UI browser UI assets without republishing bundles for every JS/CSS edit"
entry_type: reference
version: "1.0.0"
```

# Browser UI Development Workflow

Use this when working on the RyeOS UI browser UI. The goal is to avoid the
slow bundle/reinstall loop for ordinary JavaScript and CSS changes.

## Where the browser UI lives

| Piece | Path |
|---|---|
| Static HTML shell | `crates/clients/web/pkg/index.html` |
| Boot script | `crates/clients/web/pkg/bootstrap.js` |
| RyeOS UI JS shell | `crates/clients/web/pkg/ryeos_shell.js` |
| DOM renderer | `crates/clients/web/pkg/ryeos_dom_adapter.js` |
| Browser effects | `crates/clients/web/pkg/ryeos_effects.js` |
| Ambient animation | `crates/clients/web/pkg/ryeos_ambient_scene.js` |
| CSS | `crates/clients/web/pkg/web-shell.css` |
| Rust/WASM RyeOS UI model | `crates/clients/base/src/ui/` and `crates/clients/web/src/wasm.rs` |
| Static asset provider | `crates/daemon/ryeos-ui/src/assets.rs` |

## Fast JS/CSS loop without touching the daemon

Use the local dev proxy. It serves `/ui` and `/ui/assets/*` from
`crates/clients/web/pkg`, while proxying `/ui/api/*`, launch, and session
requests to the already-running daemon on `http://127.0.0.1:7400`.

Preferred command:

```bash
scripts/dev-ui-assets.sh --background --open
```

This starts the proxy if needed, asks `ryeos web` to mint a normal browser
launch token, rewrites that one-shot launch URL through the dev proxy, and
opens it. Use this instead of opening `http://127.0.0.1:7411/ui` directly when
the browser does not already have a valid `ryeos_session` cookie for the dev
proxy port.

To start the proxy without opening a fresh session:

```bash
scripts/dev-ui-assets.sh --background
```

Then open:

```text
http://127.0.0.1:7411/ui
```

Iterate like this:

1. edit files under `crates/clients/web/pkg/`;
2. refresh the browser tab at `http://127.0.0.1:7411/ui`;
3. repeat.

Stop the proxy:

```bash
scripts/dev-ui-assets.sh --stop
```

Use a different port or upstream daemon if needed:

```bash
scripts/dev-ui-assets.sh --background --port 7412 --upstream http://127.0.0.1:7400
```

This is the preferred browser asset workflow because it does **not** stop,
restart, rebuild, or reinitialize the active RyeOS daemon.

## Daemon-side asset override

Use the dev asset override. This starts the daemon with
`RYEOS_UI_ASSET_DIR` pointing at `crates/clients/web/pkg`, so `/ui` and
`/ui/assets/*` are served directly from the checkout.

```bash
scripts/dev-ui-assets.sh --direct-start
```

Do **not** run `scripts/pkg/install-local-direct.sh` or
`scripts/populate-bundles.sh` for every JS/CSS tweak.

Manual equivalent:

```bash
RYEOS_UI_ASSET_DIR=/home/leo/projects/ryeos-next/crates/clients/web/pkg ryeos start
```

Print the exact env var:

```bash
scripts/dev-ui-assets.sh --print-env
```

## One-time caveat

The dev asset override is implemented in daemon Rust code. If the installed or
running daemon predates that code, do one rebuild/reinstall/restart first. After
that, UI asset edits are live from disk.

The proxy mode above does not require this daemon-side code and is safer when
another agent/user is actively using the default daemon.

Use the heavy packaged install only when you intentionally need to update the
installed binaries/bundles:

```bash
scripts/pkg/install-local-direct.sh --trust-source-publishers
```

That script runs the heavier packaged-layout path: populate bundles, install
`ryeos`/`ryeosd`, install bundle sources, run `ryeos init`, and restart an
already-running daemon.

## What requires a rebuild or bundle refresh

| Change | Required action |
|---|---|
| `crates/clients/web/pkg/*.js` or `*.css` only | run `scripts/dev-ui-assets.sh --background`, then refresh browser at port 7411 |
| `crates/clients/base/src/ui/*` | rebuild WASM / update `crates/clients/web/pkg/ryeos_web*.{js,wasm}` through the project’s WASM build path |
| `crates/daemon/ryeos-ui/src/assets.rs` or route/static-mode code | rebuild/restart daemon |
| Bundle YAML or bundle-owned binaries | `scripts/populate-bundles.sh` or `scripts/gate.sh --no-tests` |
| Full packaged install repair | `scripts/pkg/install-local-direct.sh --trust-source-publishers` |

## Lightweight checks

For browser asset edits, prefer cheap checks:

```bash
node --check crates/clients/web/pkg/ryeos_ambient_scene.js
node --check crates/clients/web/pkg/ryeos_dom_adapter.js
```

Avoid broad `cargo test`, `cargo build --release`, or local reinstall loops
unless the change actually touches Rust, WASM, bundle artifacts, or installed
daemon behavior.

## Ambient scene state hooks

The RyeOS UI scene model is the bridge between RyeOS state and the animation.
`crates/clients/base/src/ui/scene_model.rs` emits semantic objects such as:

- `local_node`
- `remote_node`
- `project_core`
- `space_ring`
- `item_cluster`
- `thread_flow`
- `schedule_pulse`
- `service_beacon`

`crates/clients/web/pkg/ryeos_ambient_scene.js` consumes those objects and can
map counts/tone/state into visual scale, color, opacity, pulse rate, stream
count, orbit count, etc.

Future UI work should keep this separation:

```text
Rust RyeOS UICore data
  -> scene_model.rs emits semantic scene objects
  -> ryeos_ambient_scene.js maps objects to visuals
  -> browser refresh shows JS/CSS changes immediately in dev asset mode
```

Do not hardcode daemon fetches inside the animation if the data already belongs
in the RyeOS UI model. Prefer adding semantic fields/objects to the scene model
and keeping the animation as a renderer of that state.

## Common mistakes

- Running `scripts/pkg/install-local-direct.sh` for every CSS/JS edit.
- Stopping the default daemon while another agent/user is doing work; use proxy
  mode instead.
- Rebuilding bundles for changes that only touch `crates/clients/web/pkg`.
- Forgetting to restart the daemon after changing the env var.
- Expecting `RYEOS_UI_ASSET_DIR` to affect Rust/WASM model changes; it only
  serves already-built files from `pkg/`.
- Adding direct daemon API fetches to the Three.js animation instead of using
  the RyeOS UI scene model.
