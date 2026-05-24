# Client / Surface / UI Boundary Hardening — Implementation Plan

Date: 2026-05-24
Branch: `next`
Current base at time of writing: `0a486bc4 slice-9: fix pre-existing Config init and bundle_parity test, audit workspace test failures`
Next planned train after this: `/home/leo/projects/ryeos-next-descriptor-validation/.tmp/descriptor-instance-validation/00-implementation-plan.md`

This document replaces the previous client/surface next-run plan. The original
client/surface corrective train is complete through all feature slices. This
plan is a short boundary-hardening train before descriptor-instance validation.

## 0. Purpose

The remaining architectural issue is not product behavior; it is boundary
cleanliness.

We want:

- `crates/services/api` to be generic daemon HTTP / route substrate.
- `crates/services/ui` to own UI/browser concerns.
- `crates/core/app` to own domain/application state only.
- `crates/clients/*` to own renderer/client code.
- future `web` extraction into its own bundle to be plausible instead of
  blocked by hardcoded API/core coupling.

The hard constraint for this plan:

> **No UI-specific stuff in `crates/core/*`.**

Specifically, `crates/core/app/src/ui_session.rs` must be deleted by the end of
this plan, and no equivalent browser/session/UI abstraction may be reintroduced
under `crates/core/*`.

## 1. Current ground truth

Completed recent commits:

- `3cdd3379` — package rename: `ryeos-tui-terminal` → `ryeos-ui-terminal`,
  `ryeos-tui-web` → `ryeos-ui-web`.
- `0d6dbf87` — route streams carry `RouteStreamEnvelope`; `event_stream_mode` is
  the only SSE framer.
- `77d5e50b` — `/ui` and `/ui/assets/{asset}` serve embedded assets; inline
  `body_b64` route was removed.
- `62d0ad3a` — `web` binary parses launch args, signs requests, calls
  `ui.launch.mint`, and opens daemon-returned `launch_url`.
- `0a486bc4` — full workspace audit and pre-existing test fixes.

Current crate layout:

```text
crates/services/api       generic HTTP route substrate
crates/services/ui        UI/browser handlers, auth, session events, asset code
crates/clients/base       shared renderer/client model
crates/clients/terminal   terminal UI client package `ryeos-ui-terminal`, bin `ryeos-tui`
crates/clients/web        web UI client package `ryeos-ui-web`, bin `web`
crates/bin/cli            user-PATH binary `ryeos`
crates/bin/daemon         user-PATH binary `ryeosd`
```

Bundle rule:

- `crates/bin/*` is for user-PATH binaries only.
- Bundle-shipped renderer/client binaries live in their renderer/client crate as
  `[[bin]]` targets.
- `client:ryeos/web` uses `binary_ref: bin/{triple}/web`.
- There should be no live `ryeos-web-launcher` / `web-launcher` references.

Current boundary problems to fix:

1. `crates/core/app/src/ui_session.rs` defines UI/browser-specific traits and
   DTOs (`BrowserSession`, `LaunchContext`, `BrowserSessionStoreApi`,
   `SessionBusApi`, noop UI stores). This violates the hard constraint.
2. `AppState` carries `browser_sessions` and `session_bus` fields. Those are UI
   state, not core app state.
3. API route compilation still has API-owned assumptions for extension lookup.
   It should compile against composition-root registries, not hardcoded
   `ryeos_api::handlers::ALL`-style tables.
4. Static asset support should be generic in API, but web asset ownership should
   be provided by UI/composition, not hardcoded in API.
5. `session_events` should be registered as a UI stream source rather than a
   hardcoded API stream source if doing so stays small and focused.

## 2. Alignment with descriptor-instance validation

After this boundary train, descriptor-instance validation starts from:

`/home/leo/projects/ryeos-next-descriptor-validation/.tmp/descriptor-instance-validation/00-implementation-plan.md`

That plan makes the engine the descriptor contract authority:

- nested contracts
- string enums
- typed sequence elements
- post-composition validation
- structured `items.effective` validation errors
- bundle-verify lint/error output
- standard/core schema migration from live descriptor shapes

This boundary train must **not** implement descriptor validation. It must prepare
for it by making ownership lines clean:

- `client` kind schema migration later can validate `client.launch.mode`,
  `client.launch.binary_ref`, and `client.serves` without depending on
  core-tools or API-specific value gates.
- `service` kind schema migration later can validate service descriptor shape,
  while route compilation here already uses composed service descriptor
  registries instead of API-local handler lists.
- `surface` and UI/client descriptors remain owned by standard/UI/client layers,
  not by `crates/core/*`.
- Do not add new shape/value gates in binaries unless they are policy/dispatch
  checks. Descriptor typo detection belongs to the descriptor-validation train.

## 3. Process rules

1. Work stays on `next`. No feature branches.
2. One slice = one focused commit.
3. Tests first where behavior changes.
4. No backwards compatibility paths, aliases, legacy refs, or deprecation shims.
5. No UI-specific code under `crates/core/*` by the end of this train.
6. Keep `cargo check --workspace` clean and introduce no new warnings.
7. If a slice touches bundles, republish and verify both bundles.
8. If a change reveals descriptor-contract needs, write a note for the
   descriptor-validation train; do not implement validation here.

Bundle verification command with isolated roots:

```sh
tmp=$(mktemp -d)
mkdir -p "$tmp/user/.ai/config/keys/trusted" "$tmp/system/.ai"
printf '%s' 'sDKyQ9rFxIduNjGtXq6aTrLlAg39177NzCT1+YYqpRk=' \
  > "$tmp/user/.ai/config/keys/trusted/741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea.pub"
USER_SPACE="$tmp/user" RYEOS_SYSTEM_SPACE_DIR="$tmp/system" \
  target/release/ryeos-core-tools bundle-verify bundles/core
USER_SPACE="$tmp/user" RYEOS_SYSTEM_SPACE_DIR="$tmp/system" \
  target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core
rm -rf "$tmp"
```

## 4. Slice order

| Slice | Commit prefix | Goal |
|---|---|---|
| B0 | `slice-boundary-0:` | Make API route compilation consume composition-root service descriptors. |
| B1 | `slice-boundary-1:` | Move UI session state out of `AppState`; delete `core/app/src/ui_session.rs`. |
| B2 | `slice-boundary-2:` | Move web asset ownership out of API behind injected static asset providers. |
| B3 | `slice-boundary-3:` | Make stream sources composition-root driven; register `session_events` from UI. |
| B4 | `slice-boundary-4:` | Boundary audit, docs, tests, and descriptor-validation handoff check. |

If B0–B3 can be safely combined without making review difficult, keep them as
separate commits anyway. The point is to make each boundary move reviewable.

---

## Slice B0 — composition-root service descriptor lookup

### Goal

Route compilation validates `service:` refs against the same composed service
descriptor set used at runtime, not against API-local handler tables.

### Why first

This removes the asymmetric extension seam before moving more UI state behind
composition. It also aligns with the descriptor-validation plan's later `service`
kind migration: service descriptors become data contracts, not API-local Rust
module assumptions.

### Files

- `crates/services/api/src/routes/invokers/mod.rs`
- `crates/services/api/src/routes/mod.rs`
- `crates/services/api/src/registry.rs`
- `crates/services/ui/src/lib.rs`
- `crates/bin/daemon/src/main.rs`
- `crates/bin/daemon/src/uds/server.rs`
- tests under `crates/services/api` and/or `crates/services/ui`

### Implementation

1. Extend the route extension/composition registry to include service
   descriptors or a service lookup:
   ```rust
   pub struct RouteExtensionRegistry {
       pub auth: AuthInvokerRegistry,
       pub services: ServiceDescriptorRegistry,
       // existing or future extension fields
   }
   ```
   Keep the shape minimal; a borrowed descriptor slice or `Arc<[ServiceDescriptor]>`
   is enough if it avoids lifetime pain.
2. Update `compile_canonical_ref_invoker` / service-ref compile path to validate
   against the provided service descriptors.
3. API built-in tests can keep using API-only descriptors via a default registry.
4. Daemon composition must pass the composed descriptor set:
   - `ryeos_api::handlers::ALL`
   - `ryeos_ui::handlers::ALL`
5. Runtime service registry and route compile registry must be built from the
   same descriptor source. Avoid two different hand-built lists.
6. Do not move UI session state in this slice.

### Tests first

- API route compile rejects an unknown `service:` ref using a supplied descriptor
  registry.
- API route compile accepts a non-API extension service when its descriptor is in
  the supplied registry.
- Existing API service-ref compile tests still pass.
- A UI route using `service:ui/session/current` compiles only when UI descriptors
  are included.

### Verify

```sh
cargo test -p ryeos-api routes::invokers
cargo test -p ryeos-api routes
cargo test -p ryeos-ui
cargo check --workspace
```

### Commit

```sh
git commit -m "slice-boundary-0: compile service refs from composed descriptors"
```

---

## Slice B1 — remove UI session state from `core/app`

### Goal

Delete `crates/core/app/src/ui_session.rs` and remove UI/browser session fields
from `AppState`. UI state lives entirely in `crates/services/ui` and is injected
into UI handlers/invokers at the daemon composition root.

This is the hard-constraint slice.

### Files

- delete `crates/core/app/src/ui_session.rs`
- `crates/core/app/src/lib.rs`
- `crates/core/app/src/state.rs`
- `crates/core/app/src/stream_envelope.rs` (stays in core/app)
- `crates/services/ui/src/lib.rs`
- add `crates/services/ui/src/state.rs`
- `crates/services/ui/src/browser_session.rs`
- `crates/services/ui/src/session_bus.rs`
- `crates/services/ui/src/handlers/*`
- `crates/services/ui/src/invokers/*`
- `crates/services/api/src/routes/invocation.rs` only if context needs an
  extension bag
- `crates/bin/daemon/src/main.rs`
- `crates/bin/daemon/src/uds/server.rs`
- tests under `crates/services/api`, `crates/services/ui`, daemon test helpers

### Target shape

`crates/services/ui/src/state.rs`:

```rust
#[derive(Clone)]
pub struct UiState {
    pub browser_sessions: Arc<BrowserSessionStore>,
    pub session_bus: Arc<SessionBus>,
}
```

`AppState` must no longer contain:

```rust
browser_sessions
session_bus
```

`crates/core/app/src/lib.rs` must no longer export `ui_session`.

### Implementation

1. Add `UiState` to `ryeos-ui`.
2. Change UI handlers to receive `Arc<UiState>` by closure capture or explicit
   handler wrapper, rather than reading `state.browser_sessions` or
   `state.session_bus`.
3. Change UI auth/stream invokers to hold `Arc<UiState>`:
   ```rust
   pub struct CompiledBrowserSessionVerifier { ui: Arc<UiState> }
   pub struct CompiledSessionEventsInvocation { ui: Arc<UiState>, keep_alive_secs: u64 }
   ```
4. Adjust service handler registration so UI service handlers can capture
   `Arc<UiState>`. If the existing `ServiceDescriptor` requires fn pointers,
   split descriptor metadata from runtime handler registration:
   - descriptor metadata remains static and data-like
   - runtime registry stores `Arc<dyn Fn(...)>` handlers
   - API handlers can still register plain function handlers
   - UI handlers register closures capturing `UiState`
5. Remove `browser_sessions` and `session_bus` from `AppState` construction in
   daemon and tests.
6. Delete `crates/core/app/src/ui_session.rs`.
7. Replace API test noop UI stores with either no fields or API-local test
   extension state. There should be no `NoopBrowserSessionStore` in core.
8. Keep `RouteStreamEnvelope` in `crates/core/app/src/stream_envelope.rs`. It is
   transport-neutral and not UI-specific.

### Tests first

- `rg 'ui_session|BrowserSession|BrowserSessionStore|SessionBus|browser_sessions|session_bus' crates/core` returns no UI-specific matches except generic comments only if unavoidable. Prefer zero matches for all listed terms except `RouteStreamEnvelope`.
- UI handlers still pass:
  - `cargo test -p ryeos-ui ui_launch`
  - `cargo test -p ryeos-ui ui_session_current`
  - `cargo test -p ryeos-ui ui_actions_invoke`
- API tests still build without UI noop state in `AppState`.
- Daemon state construction compiles.

### Verify

```sh
cargo test -p ryeos-ui
cargo test -p ryeos-api
cargo check --workspace
rg 'ui_session|BrowserSession|BrowserSessionStore|SessionBus|browser_sessions|session_bus' crates/core
```

The final `rg` should show **no UI-specific core usage**. If it finds a real UI
reference under `crates/core/*`, the slice is not done.

### Commit

```sh
git commit -m "slice-boundary-1: move UI session state out of core app"
```

---

## Slice B2 — injected static asset providers

### Goal

`ryeos-api` owns generic static response mechanics, but not web asset bytes or
`clients/web/pkg` knowledge. UI/composition provides the web asset provider.

### Current problem

`crates/services/api` currently has API-owned embedded asset code that knows the
web client asset layout. That couples generic API substrate to the web client and
makes future web-bundle extraction harder.

### Files

- `crates/services/api/src/routes/response_modes/static_mode.rs`
- `crates/services/api/src/routes/mod.rs`
- current API embedded asset module (move/delete from API)
- add/move to `crates/services/ui/src/assets.rs` or similar
- `crates/services/ui/src/lib.rs`
- `crates/bin/daemon/src/main.rs`
- `crates/bin/daemon/src/uds/server.rs`
- `crates/services/api/tests/routes_ui.rs` or split API/UI tests as needed

### Target shape

API defines generic provider traits/types, with no web-specific paths:

```rust
pub struct StaticAsset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
    pub etag: &'static str,
    pub cache_control: &'static str,
}

pub trait StaticAssetProvider: Send + Sync {
    fn get(&self, path: &str) -> Option<StaticAsset>;
}
```

The route/response extension registry supplies named providers:

```rust
embedded_asset -> Arc<dyn StaticAssetProvider>
```

`ryeos-ui` owns the concrete web provider that embeds
`crates/clients/web/pkg/*`.

### Implementation

1. Move web asset bytes and path manifest out of `ryeos-api` and into
   `ryeos-ui`.
2. Keep `static_mode` support for `source: embedded_asset`, but resolve that
   source through an injected provider registry.
3. Validate route specs at compile time only against provider existence and
   literal asset existence when the path is literal. Dynamic path captures are
   runtime 404s when missing.
4. Preserve current headers and ETag/304 behavior.
5. Do not introduce bundle-backed asset loading yet unless it is smaller than
   keeping the existing embedded provider. The goal is ownership inversion, not
   a new asset delivery system.

### Tests first

- API static mode can serve an injected fake asset provider.
- API static mode rejects `source: embedded_asset` when no provider is registered.
- UI/web provider serves `index.html` and an asset with existing headers.
- `/ui` and `/ui/assets/{asset}` behavior remains unchanged from Slice 7.
- `rg 'clients/web|pkg/index.html|bootstrap.js|embedded web' crates/services/api`
  shows no web-specific asset ownership in API.

### Verify

```sh
cargo test -p ryeos-api static_mode
cargo test -p ryeos-api --test routes_ui
cargo test -p ryeos-ui
cargo check --workspace
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
# verify both bundles with isolated roots
```

### Commit

```sh
git commit -m "slice-boundary-2: inject UI static asset provider"
```

---

## Slice B3 — stream source registry

### Goal

`event_stream_mode` should not hardcode UI-specific source names. API owns the
transport/framing mode; UI registers `session_events` as a stream source.

### Files

- `crates/services/api/src/routes/response_modes/event_stream_mode.rs`
- `crates/services/api/src/routes/response_modes/mod.rs`
- `crates/services/api/src/routes/mod.rs`
- `crates/services/ui/src/lib.rs`
- `crates/services/ui/src/invokers/session_events_invocation.rs`
- tests under API/UI

### Implementation

1. Define a stream source registry used by `event_stream_mode` during route
   compile.
2. Built-in API sources remain registered by API:
   - `dispatch_launch`
   - `thread_events`
3. UI registers:
   - `session_events`
4. `event_stream_mode` should validate generic stream source contract and then
   delegate source-specific validation/invoker construction to the registered
   source compiler.
5. Preserve current `session_events` route YAML. This is an internal compiler
   boundary change, not a route protocol change.

### Tests first

- API event stream mode rejects unknown sources through registry lookup.
- Built-in `dispatch_launch` and `thread_events` still compile.
- `session_events` compiles only when UI stream source registry is included.
- Existing session-events tests still pass.

### Verify

```sh
cargo test -p ryeos-api event_stream
cargo test -p ryeos-ui session_events
cargo check --workspace
```

If route specs or bundles change, republish and verify bundles.

### Commit

```sh
git commit -m "slice-boundary-3: register UI stream sources outside API"
```

---

## Slice B4 — boundary audit and descriptor-validation handoff

### Goal

Prove the boundaries are clean enough to start descriptor-instance validation.

### Required audit checks

Run and record results in the commit message or a small `.tmp` note if useful:

```sh
# no UI-specific code in core
rg 'ui_session|BrowserSession|BrowserSessionStore|SessionBus|browser_sessions|session_bus' crates/core

# no web asset ownership in API
rg 'clients/web|crates/clients/web|pkg/index.html|bootstrap.js|ryeos-ui-web' crates/services/api

# no stale old names
rg 'ryeos-web-launcher|web-launcher|ryeos-tui-web|ryeos-tui-terminal' . -g '!target'

# expected current package names
rg 'ryeos-ui-web|ryeos-ui-terminal|ryeos-client-base' Cargo.toml crates scripts bundles
```

Expected:

- first command: no real UI-specific core references
- second command: no web asset ownership in API
- third command: zero stale live-code references; historical notes under ignored
  `.tmp` are okay only if intentionally kept and not part of the active plan
- fourth command: finds current package names where expected

### Descriptor-validation alignment checklist

Before handing off to descriptor validation, confirm:

1. `client:ryeos/web` still uses `binary_ref: bin/{triple}/web`.
2. API route compile service-ref lookup is descriptor-list driven.
3. UI service descriptors are part of the composed service descriptor set.
4. No Rust value-shape gates were added for descriptor typo detection.
5. `crates/core/*` contains no UI/browser session state or traits.
6. `ryeos-api` does not own web asset bytes.
7. `items.effective` behavior was not changed except through generic registry
   plumbing if unavoidable.
8. Bundle verify passes for core and standard.

### Verify

```sh
cargo check --workspace
cargo test -p ryeos-api
cargo test -p ryeos-ui
cargo test -p ryeos-ui-web
cargo test -p ryeos-ui-terminal
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
# verify both bundles with isolated roots
```

### Commit

```sh
git commit -m "slice-boundary-4: audit UI/API/core boundaries"
```

---

## End state before descriptor validation

```text
crates/core/app
  domain AppState only
  stream_envelope remains transport-neutral
  no UI/browser session contracts or noop UI stores

crates/services/api
  route matching/compilation substrate
  invocation contracts
  response modes and HTTP framing
  generic service/static/stream extension registries
  no web asset bytes
  no UI handler ownership

crates/services/ui
  UiState
  BrowserSessionStore
  SessionBus
  browser_session auth source
  session_events stream source
  /ui service handlers
  web static asset provider

crates/clients/web
  renderer/client package
  binary target `web`
  web pkg assets as client-owned inputs

crates/bin/daemon
  composition root wiring app + api + ui
```

At that point the descriptor-validation plan can safely make the engine the
source of truth for descriptor contracts without fighting hidden UI/core/API
coupling.
