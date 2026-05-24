# Client / Surface / UI Boundary Finalization — Advanced Implementation Plan

Date: 2026-05-24
Branch: `next`
Current base at time of writing: `a8014321 slice-boundary-4: audit UI/API/core boundaries`
Next planned train after this: `/home/leo/projects/ryeos-next-descriptor-validation/.tmp/descriptor-instance-validation/00-implementation-plan.md`

This document is the final boundary-polish plan before descriptor-instance
validation. The client/surface feature train and the first boundary-hardening
train are complete. This plan covers the remaining advanced cleanup: remove the
last UI-specific helper hooks from generic API substrate and fix the one service
availability mismatch found by review.

## 0. Non-negotiable constraints

1. **No UI-specific code under `crates/core/*`.** This is already true after
   `05e75a00`; do not regress it.
2. `crates/services/api` owns generic route substrate only:
   - route matching/compilation
   - invocation contracts
   - response modes and HTTP framing
   - generic registries for service descriptors, auth verifiers, stream sources,
     static asset providers, and extension state
3. `crates/services/ui` owns browser/UI concerns:
   - `UiState`
   - browser sessions
   - session bus
   - `browser_session` auth verifier
   - `session_events` stream source
   - `/ui` handlers
   - web asset provider
4. `crates/bin/daemon` is the composition root. It wires API built-ins plus UI
   extensions together.
5. Do **not** implement descriptor-instance validation in this train. Descriptor
   shape/value enforcement starts in the descriptor-validation worktree after
   this plan lands.
6. No backwards compatibility paths, legacy aliases, or support-both code paths.

## 1. Current completed state

Completed boundary commits:

| Commit | Slice | Result |
|---|---|---|
| `91af8144` | boundary-0 | Route compilation uses composition-root service descriptors. |
| `05e75a00` | boundary-1 | UI session state moved out of `core/app`; `core/app/src/ui_session.rs` deleted. |
| `cc580e56` | boundary-2 | Static assets are injected; API no longer owns web asset bytes. |
| `e36e6b57` | boundary-3 | Event stream mode has a stream source registry; UI registers `session_events`. |
| `a8014321` | boundary-4 | Boundary audit documented; core/API/UI split verified. |

Current intended ownership:

```text
crates/core/app
  domain AppState only
  transport-neutral stream_envelope only
  no UI/browser session state or contracts

crates/services/api
  generic route substrate
  service descriptor lookup plumbing
  auth/source/static registries
  HTTP response framing
  no web bytes
  no UI state ownership

crates/services/ui
  UiState
  BrowserSessionStore
  SessionBus
  browser_session auth verifier
  session_events source
  /ui service handlers
  web static asset provider

crates/clients/web
  package ryeos-ui-web
  bundled binary target web
  client-owned web pkg assets

crates/bin/daemon
  composition root
```

## 2. Remaining review findings

Final Oracle review found one concrete bug and several API-shape smells.

### Must-fix bug

`crates/services/ui/src/handlers/ui_launch.rs` declares
`ServiceAvailability::Both`, but it depends on daemon/UI composition:

- requires injected `UiState`
- is meaningful only through daemon `/ui/launch` route cookie/redirect semantics
- standalone service mode has no `UiState`

It must be `DaemonOnly`.

### Advanced cleanup worth doing now

The following API APIs still expose UI-shaped helper names:

- `AuthInvokerRegistry { browser_session: Option<_> }`
- `ResponseModeRegistry::with_builtins_and_session_events(...)`
- `ResponseModeRegistry::with_builtins_and_session_events_from(...)`
- any `with_session_events(...)` constructor/helper
- single-purpose `StaticMode::with_provider(...)` or optional provider fields if
  they imply only one static provider
- `AppState::service_extensions: Option<Arc<dyn Any + Send + Sync>>` as a single
  opaque extension object

These are not behavior bugs, but they are exactly the kind of accidental seam
that will become annoying during descriptor-instance validation and future web
bundle extraction. Clean them now while the surface is small.

## 3. Alignment with descriptor-instance validation

The descriptor-validation plan will make kind schemas and the engine the source
of truth for descriptor shape/value contracts:

- `client.launch.mode` enum validation
- `client.launch.binary_ref` required field validation
- service descriptor shape validation
- structured `items.effective` contract violations
- bundle-verify hard errors and warnings

This boundary finalization must provide a clean substrate for that work:

- service descriptors are composed data at the daemon root, not API-local Rust
  assumptions
- route sources/auth/static providers are registry entries, not special-case
  methods named after UI concepts
- UI-specific names (`browser_session`, `session_events`, `embedded_asset`) exist
  as registrations from `ryeos-ui` and route YAML values, not as hardcoded API
  struct fields or constructor names
- no descriptor typo/value validation is added here; leave that to the next train

## 4. Process / verification rules

1. Work stays on `next`.
2. One slice = one commit.
3. Tests first where behavior changes.
4. `cargo check --workspace` must stay clean.
5. If a slice touches bundles, republish and verify both bundles:

```sh
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev

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

## 5. Slice order

| Slice | Commit prefix | Goal |
|---|---|---|
| BF | `slice-boundary-fix:` | Mark `ui.launch` daemon-only and add an availability regression test. |
| B5 | `slice-boundary-5:` | Replace UI-specific helper hooks with generic extension registries. |
| B6 | `slice-boundary-6:` | Final audit and descriptor-validation handoff check. |

---

## Slice BF — mark `ui.launch` daemon-only

### Goal

Make the service descriptor contract match reality: `ui.launch` is daemon-only.

### Files

- `crates/services/ui/src/handlers/ui_launch.rs`
- UI service descriptor tests, likely under `crates/services/ui/tests/` or module
  tests in `crates/services/ui/src/handlers/mod.rs`

### Implementation

1. Change `ui_launch::DESCRIPTOR.availability` from `ServiceAvailability::Both`
   to `ServiceAvailability::DaemonOnly`.
2. Add a regression test:
   - direct test: `ui_launch_descriptor_is_daemon_only`
   - or broader invariant: UI descriptors requiring `UiState` are not `Both`
3. Do not alter runtime launch behavior.

### Verify

```sh
cargo test -p ryeos-ui ui_launch
cargo test -p ryeos-ui
cargo check --workspace
```

### Commit

```sh
git commit -m "slice-boundary-fix: mark ui.launch daemon-only"
```

---

## Slice B5 — generic route extension registries

### Goal

Remove the remaining UI-specific helper hooks from `ryeos-api` and replace them
with generic registries. After this slice, API substrate should have no methods
or struct fields named after UI concepts such as `browser_session` or
`session_events`.

### Non-goals

- No dynamic plugin loading.
- No separate web bundle extraction.
- No descriptor validation.
- No route YAML protocol changes.
- No behavior changes to `/ui` routes.

### Target end state

`ryeos-api` exposes generic registry types:

```rust
pub struct AuthInvokerRegistry {
    verifiers: HashMap<String, Arc<dyn CompiledRouteInvocation>>,
}

pub struct StreamSourceRegistry {
    sources: HashMap<String, Arc<dyn StreamSourceCompiler>>,
}

pub struct StaticAssetProviderRegistry {
    providers: HashMap<String, Arc<dyn StaticAssetProvider>>,
}

pub struct ExtensionState {
    entries: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}
```

Exact names can differ, but the architecture must match:

- API registers API built-ins by name.
- UI registers UI sources/providers/verifiers by name.
- Daemon composition root calls both registrations.
- API does not expose `with_session_events`, `browser_session: Option<_>`, or a
  single `service_extensions: Option<Any>` slot.

### Files

Likely files:

- `crates/services/api/src/routes/invokers/mod.rs`
- `crates/services/api/src/routes/response_modes/event_stream_mode.rs`
- `crates/services/api/src/routes/response_modes/static_mode.rs`
- `crates/services/api/src/routes/response_modes/mod.rs`
- `crates/services/api/src/routes/mod.rs`
- `crates/services/api/src/routes/invocation.rs`
- `crates/core/app/src/state.rs`
- `crates/services/ui/src/lib.rs`
- `crates/services/ui/src/state.rs`
- `crates/services/ui/src/assets.rs`
- `crates/services/ui/src/invokers/browser_session_invocation.rs`
- `crates/services/ui/src/invokers/session_events_invocation.rs`
- `crates/bin/daemon/src/main.rs`
- `crates/bin/daemon/src/uds/server.rs`
- tests under `crates/services/api` and `crates/services/ui`

### Part A — auth verifier registry

Replace any UI-specific field like:

```rust
pub struct AuthInvokerRegistry {
    pub browser_session: Option<Arc<dyn CompiledRouteInvocation>>,
}
```

with a generic map/registry keyed by auth name:

```rust
pub struct AuthInvokerRegistry {
    verifiers: HashMap<String, Arc<dyn CompiledRouteInvocation>>,
}
```

API registers built-ins:

- `none`
- `ryeos_signed`
- `hmac` (if hmac needs config, register a compiler/factory rather than a fixed
  invoker)

UI registers:

- `browser_session`

Route auth compilation does generic lookup by `raw.auth`.

Tests:

- `none` / `ryeos_signed` / `hmac` still compile as before.
- `browser_session` is unknown with API-only registry.
- `browser_session` compiles when UI registers it.
- Unknown auth name still errors clearly.

### Part B — stream source registry

Replace helpers like:

```rust
ResponseModeRegistry::with_builtins_and_session_events(...)
EventStreamMode::with_session_events(...)
```

with generic stream-source registration:

```rust
pub trait StreamSourceCompiler: Send + Sync {
    fn compile(&self, raw: &RawRouteSpec) -> Result<EventStreamStrategy, RouteConfigError>;
}

pub struct StreamSourceRegistry {
    sources: HashMap<String, Arc<dyn StreamSourceCompiler>>,
}
```

API registers built-in stream sources:

- `dispatch_launch`
- `thread_events`

UI registers:

- `session_events`

`event_stream_mode` does:

```rust
let source = raw.response.source.as_deref().unwrap_or("");
let compiler = stream_sources.get(source).ok_or_unknown_source(...)?;
let strategy = compiler.compile(raw)?;
```

No method or constructor in API should contain `session_events` in its name.
It is acceptable for API tests to mention the string `session_events` only when
asserting that it is unknown without UI registration.

Tests:

- API-only mode compiles `dispatch_launch` and `thread_events`.
- API-only mode rejects `session_events` as unknown.
- UI-composed mode compiles `session_events`.
- Existing session-events behavior remains unchanged.

### Part C — static asset provider registry

Replace one-off optional provider shapes like:

```rust
StaticMode { asset_provider: Option<Arc<dyn StaticAssetProvider>> }
StaticMode::with_provider(...)
```

with a generic provider registry:

```rust
pub struct StaticAssetProviderRegistry {
    providers: HashMap<String, Arc<dyn StaticAssetProvider>>,
}
```

Route YAML remains:

```yaml
response:
  mode: static
  source: embedded_asset
```

API static mode resolves `embedded_asset` through the registry. UI registers the
provider named `embedded_asset`.

Tests:

- fake provider registered as `embedded_asset` serves a test asset.
- `source: embedded_asset` rejects when provider is not registered.
- unknown static source rejects clearly.
- UI web asset provider behavior remains unchanged.

### Part D — typed extension state bag

Replace the single slot:

```rust
service_extensions: Option<Arc<dyn Any + Send + Sync>>
```

with a typed extension bag:

```rust
pub struct ExtensionState {
    entries: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl ExtensionState {
    pub fn insert<T: Any + Send + Sync>(&mut self, value: Arc<T>);
    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>>;
}
```

`AppState` can keep a generic field such as:

```rust
pub extensions: Arc<ExtensionState>
```

This remains generic and is not UI-specific. `ryeos-ui` can provide a helper:

```rust
pub fn get_ui_state(state: &AppState) -> Option<Arc<UiState>> {
    state.extensions.get::<UiState>()
}
```

Tests:

- extension bag round-trips a typed `Arc<T>`.
- missing extension returns `None`.
- UI tests retrieve `UiState` through the typed bag.

### Part E — composition root API

Create one clear composition path in daemon code. Names can vary, but the shape
should be easy to audit:

```rust
let ui_state = Arc::new(ryeos_ui::UiState::new());

let mut route_extensions = ryeos_api::routes::RouteExtensionRegistry::with_api_builtins(
    service_descriptors,
);
let mut response_modes = ryeos_api::routes::response_modes::ResponseModeRegistry::with_api_builtins();
let mut extension_state = ryeos_api::extensions::ExtensionState::new();

ryeos_ui::register_extensions(
    &mut route_extensions,
    &mut response_modes,
    &mut extension_state,
    ui_state,
);
```

Avoid multiple competing helper paths. The daemon should compose once and pass
the composed registries into route-table build and service-registry build.

### Required audit after implementation

These should have no live-code matches outside tests/docs explicitly asserting
absence:

```sh
rg 'with_builtins_and_session_events|with_session_events' crates/services/api crates/services/ui crates/bin/daemon
rg 'browser_session: Option' crates/services/api crates/services/ui crates/bin/daemon
rg 'service_extensions: Option' crates/core crates/services crates/bin/daemon
rg 'asset_provider: Option' crates/services/api/src/routes
```

These strings may still appear as route/auth names, YAML values, test fixtures,
or UI registration keys:

- `browser_session`
- `session_events`
- `embedded_asset`

That is fine. The goal is not to remove the route protocol names; it is to stop
encoding them as API struct fields or constructor names.

### Verify

```sh
cargo test -p ryeos-api routes
cargo test -p ryeos-api event_stream
cargo test -p ryeos-api static_mode
cargo test -p ryeos-ui
cargo check --workspace
```

If bundles are touched:

```sh
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
# verify both bundles with isolated roots
```

### Commit

```sh
git commit -m "slice-boundary-5: replace UI-specific hooks with generic registries"
```

---

## Slice B6 — final audit and descriptor-validation handoff

### Goal

Prove the repository is ready for descriptor-instance validation.

### Required audit commands

```sh
# hard constraint: no UI-specific core references
rg 'ui_session|BrowserSession|BrowserSessionStore|SessionBus|browser_sessions|session_bus' crates/core

# no web asset ownership in API
rg 'clients/web|crates/clients/web|pkg/index.html|bootstrap.js|ryeos-ui-web' crates/services/api

# no stale old package/binary names
rg 'ryeos-web-launcher|web-launcher|ryeos-tui-web|ryeos-tui-terminal' . -g '!target'

# no UI-specific helper hooks in API/daemon wiring
rg 'with_builtins_and_session_events|with_session_events|browser_session: Option|service_extensions: Option|asset_provider: Option' crates/services crates/core crates/bin/daemon

# expected current names still present where appropriate
rg 'ryeos-ui-web|ryeos-ui-terminal|ryeos-client-base|bin/\{triple\}/web' Cargo.toml crates scripts bundles
```

Expected:

- first command: zero real matches under `crates/core`
- second command: zero matches under `crates/services/api`
- third command: zero live-code matches; ignored historical `.tmp` content is not
  relevant if not tracked as active plan
- fourth command: zero matches
- fifth command: finds current names

### Descriptor-validation handoff checklist

Before starting `/home/leo/projects/ryeos-next-descriptor-validation/.tmp/descriptor-instance-validation/00-implementation-plan.md`, confirm:

1. `client:ryeos/web` still uses `binary_ref: bin/{triple}/web`.
2. `ui.launch` is `DaemonOnly`.
3. Service-ref route compile uses composed descriptor registries.
4. Auth names are registry entries.
5. Stream sources are registry entries.
6. Static asset providers are registry entries.
7. UI state is in `ryeos-ui`, retrieved through a generic extension bag or
   closure-captured by UI handlers/invokers.
8. `crates/core/*` contains no UI/browser state or contracts.
9. `crates/services/api` contains no web asset bytes or client-web path knowledge.
10. No Rust descriptor shape/value gates were added in this train.
11. Existing workspace known-failure audit remains valid or is updated if full
    test identity changed.
12. Core and standard bundles verify.

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
git commit -m "slice-boundary-6: audit generic route extension boundaries"
```

---

## End state before descriptor validation

```text
crates/core/app
  domain AppState only
  transport-neutral stream_envelope only
  generic extension bag allowed
  no UI/browser session contracts or noop UI stores

crates/services/api
  route matching/compilation substrate
  invocation contracts
  response modes and HTTP framing
  generic service descriptor registry
  generic auth verifier registry
  generic stream source registry
  generic static asset provider registry
  no UI-named helper constructors
  no web asset bytes

crates/services/ui
  UiState
  BrowserSessionStore
  SessionBus
  registers auth name: browser_session
  registers stream source: session_events
  registers static provider: embedded_asset
  /ui service handlers
  web static asset provider

crates/clients/web
  renderer/client package ryeos-ui-web
  binary target web
  web pkg assets as client-owned inputs

crates/bin/daemon
  composition root wiring app + api + ui registries
```

This is the correct base for descriptor-instance validation: the engine can
become the descriptor contract authority without hidden UI/core/API ownership
leaks or API helper names that encode today’s UI concepts.
