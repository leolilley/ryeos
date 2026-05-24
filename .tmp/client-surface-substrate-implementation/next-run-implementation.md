# Client / Surface / Renderer Substrate — Next Run Implementation Plan

Date: 2026-05-24
Branch: `next`
Current HEAD at time of writing: `42969e60 slice-layout: rename web client binary to web`
Mode: slice-by-slice development directly on `next`.

This document replaces the superseded v2 corrective plan. It only describes
what remains after the completed work through:

- `slice-0` through `slice-5`
- `slice-6a`
- `slice-layout`
- `slice-layout: rename web client binary to web`

## Ground truth / current layout

### Crates

- Generic daemon HTTP substrate: `crates/services/api`
- UI/browser substrate: `crates/services/ui`
- Shared renderer/client substrate: `crates/clients/base`
- Terminal renderer crate and bundled binary: `crates/clients/terminal`
  - package: `ryeos-ui-terminal`
  - binary: `ryeos-tui`
- Web renderer crate and bundled binary: `crates/clients/web`
  - package: `ryeos-ui-web`
  - binary: `web`
  - binary source: `crates/clients/web/src/bin/web.rs`

`crates/bin/*` is for user-PATH binaries only (`ryeos`, `ryeosd`). Do not put
bundle-shipped renderer binaries there. Bundle-shipped renderer binaries live
inside their renderer crate as `[[bin]]` targets.

### Bundle descriptors

- `client:ryeos/web` lives at `bundles/standard/.ai/clients/ryeos/web.yaml`.
- Its launch binary is `binary_ref: bin/{triple}/web`.
- `scripts/populate-bundles.sh` builds it with:
  `cargo build --release -p ryeos-ui-web --bin web`.

There should be no `ryeos-web-launcher` or `web-launcher` references in live
code/descriptors/scripts. If one appears, treat it as stale naming and remove it
in the same slice.

### UI session state

- `BrowserSessionStore` implementation: `crates/services/ui/src/browser_session.rs`
- `SessionBus` implementation: `crates/services/ui/src/session_bus.rs`
- Minimal traits/DTOs used by `AppState`: `crates/core/app/src/ui_session.rs`
- Transport-neutral stream DTO: `crates/core/app/src/stream_envelope.rs`

### Known current gaps

1. Streams still return `RouteEventStream` containing
   `axum::response::sse::Event` values. This means some invokers still frame SSE
   themselves. Slice 6b fixes this.
2. `/ui` is still served from `body_b64` in
   `bundles/standard/.ai/node/routes/ui_index.yaml`. Slice 7 replaces this with
   embedded assets.
3. The web binary exists as `web`, but argument propagation and descriptor/alias
   completeness are still not done. Slice 8 fixes this.
4. Full workspace test/audit remains. Slice 9 handles this.

## Non-negotiable process rules

1. Work stays on `next`. No feature branches.
2. One slice = one focused diff committed directly to `next`.
3. Tests first for behavioral slices. Tests should fail before implementation
   when practical, then pass after implementation.
4. No backwards compatibility, no legacy refs, no migration aliases, no
   deprecation shims, no "support both for one release" paths. Rename/move/delete
   the old form in the same commit and update every in-tree consumer.
5. After any bundle-affecting slice:
   ```sh
   ./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
   ```
   Then verify bundles. Use isolated roots to avoid duplicate installed-bundle
   provider state from the developer machine:
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
6. Keep `cargo check --workspace` clean. Do not introduce new warnings.
7. If a slice reveals a structural problem beyond its scope, commit what is
   clean, write a short follow-up note under `.tmp/`, and stop.

## Remaining slice order

| Slice | Commit prefix | Purpose |
|---|---|---|
| rename | `slice-rename:` | Rename `ryeos-tui-terminal` → `ryeos-ui-terminal`, `ryeos-tui-web` → `ryeos-ui-web`. TUI is terminal-only; both are UI clients. |
| 6b | `slice-6b:` | Adopt `RouteStreamEnvelope` for every stream and make `event_stream_mode` the only SSE framer. |
| 7 | `slice-7:` | Implement embedded assets and rewrite `/ui` shell away from `body_b64`. |
| 8 | `slice-8:` | Complete web launcher arg propagation and `client:ryeos/web` descriptor/alias/verb surface. |
| 9 | `slice-9:` | Full workspace test pass and engine failure identity audit. |

---

## Slice rename — package name cleanup: `ryeos-tui-*` → `ryeos-ui-*`

### Goal

Rename both client crate packages from `ryeos-tui-*` to `ryeos-ui-*`. The "TUI"
prefix only applies to the terminal renderer; both crates are UI clients.

- `ryeos-tui-terminal` → `ryeos-ui-terminal`
- `ryeos-tui-web` → `ryeos-ui-web`

Binary names (`ryeos-tui`, `web`) are **not** affected. Directory names
(`crates/clients/terminal`, `crates/clients/web`) are **not** affected.

### Files

- `crates/clients/terminal/Cargo.toml` — `name = "ryeos-ui-terminal"`
- `crates/clients/web/Cargo.toml` — `name = "ryeos-ui-web"`
- Every `Cargo.toml` or `.rs` file referencing `ryeos-tui-terminal` or
  `ryeos-tui-web` as a dependency or `extern crate`.
- `scripts/populate-bundles.sh` — build command uses `-p ryeos-ui-web`.
- Any test files or doc comments referencing the old package names.

### Implementation

1. Update `package.name` in both `Cargo.toml` files.
2. Search the entire workspace for `ryeos-tui-terminal` and `ryeos-tui-web`.
   Replace every in-tree reference. There are no backwards-compatibility
   aliases — rename and update all consumers in the same commit.
3. Update `scripts/populate-bundles.sh` build target.
4. Verify `cargo check --workspace` is clean.

### Verify

```sh
cargo check --workspace
rg -c 'ryeos-tui-terminal\|ryeos-tui-web' --type rust --type toml
# ^ should return zero matches
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
# verify both bundles with isolated roots, see process rule above
```

### Commit

```sh
git commit -m "slice-rename: ryeos-tui-* → ryeos-ui-* package names"
```

---

## Slice 6b — `RouteStreamEnvelope` adoption for all streams

### Goal

Every streaming invoker emits transport-neutral `RouteStreamEnvelope` values.
`event_stream_mode` is the only layer that converts envelopes to SSE frames.
Per-invoker SSE framing is deleted.

### Current state to change

- `crates/services/api/src/routes/invocation.rs` defines `RouteEventStream` as a
  stream of `axum::response::sse::Event`.
- `crates/services/api/src/routes/invokers/gateway_stream_invocation.rs` returns
  SSE events.
- `crates/services/api/src/routes/invokers/subscription_stream_invocation.rs`
  returns SSE events.
- `crates/services/ui/src/invokers/session_events_invocation.rs` currently
  converts envelopes to SSE internally.
- `crates/services/api/src/routes/stream_envelope.rs` already has an
  envelope-to-SSE helper that can become the single framing helper used by
  `event_stream_mode`.

### Files

- `crates/core/app/src/stream_envelope.rs`
- `crates/services/api/src/routes/stream_envelope.rs`
- `crates/services/api/src/routes/invocation.rs`
- `crates/services/api/src/routes/response_modes/event_stream_mode.rs`
- `crates/services/api/src/routes/invokers/gateway_stream_invocation.rs`
- `crates/services/api/src/routes/invokers/subscription_stream_invocation.rs`
- `crates/services/api/src/routes/invokers/stream_helpers.rs`
- `crates/services/ui/src/invokers/session_events_invocation.rs`
- `crates/services/ui/src/session_bus.rs`
- tests under `crates/services/api` and `crates/services/ui`

### Implementation

1. Change the stream result contract from SSE-event streams to envelope streams.
   The shape can keep the name `RouteEventStream` if that minimizes churn, but
   its `events` field should become:
   ```rust
   Pin<Box<dyn Stream<Item = Result<RouteStreamEnvelope, Infallible>> + Send>>
   ```
   If renaming is cleaner, use a single rename and update all in-tree users in
   the same commit.
2. Move all envelope-to-SSE framing into `event_stream_mode`.
   - `event:` = `envelope.event_type`
   - `id:` = `envelope.id` when present
   - `data:` = JSON serialization of `envelope.payload`
   - keepalive comments remain owned by Axum `Sse`/`KeepAlive`
3. Update `gateway_stream_invocation` to emit envelopes instead of SSE events.
4. Update `subscription_stream_invocation` to emit envelopes instead of SSE
   events.
5. Update `session_events_invocation` to yield the envelopes from `SessionBus`
   directly. It must not call `axum::response::sse::Event` or its own
   `envelope_to_sse` helper.
6. Delete per-invoker SSE framing helpers that become unused.
7. Finish and test `Last-Event-ID` behavior through the `SessionBus` replay
   ring:
   - known ID replays events after that ID
   - unknown/gapped ID emits `snapshot_required`
   - broadcast lag also emits `snapshot_required`
8. Seed/publish session events from the correct code paths where available:
   - action invocation already publishes `action.invoked`
   - add thread upsert / capability changed / reload available publishers only
     where the source-of-truth code path is clear; do not invent fake events
     just to satisfy tests.

### Tests first

Add or update focused tests before implementation:

- `crates/services/api`: event stream mode frames an envelope as SSE.
- `crates/services/ui`: session events invoker subscribes and yields envelopes.
- `crates/services/ui`: `Last-Event-ID` known ID replays missed envelopes.
- `crates/services/ui`: unknown/gapped ID yields `snapshot_required`.
- One existing gateway/subscription stream test is rewritten to assert envelope
  output and `event_stream_mode` SSE output. Delete old raw SSE-byte assertions.

### Verify

```sh
cargo test -p ryeos-api event_stream
cargo test -p ryeos-ui session_events
cargo check --workspace
```

If bundle route files or service descriptors change, republish and verify both
bundles with the isolated-root commands above.

### Commit

```sh
git commit -m "slice-6b: adopt RouteStreamEnvelope for route streams"
```

---

## Slice 7 — embedded assets + `/ui` shell rewrite

### Goal

`/ui` and `/ui/assets/{asset}` serve real embedded files from
`crates/clients/web/pkg/` through data-driven route specs. `ui_index.yaml` no
longer carries a `body_b64` HTML blob. The shell calls:

1. `GET /ui/api/session/current`
2. `items.effective` for `session.surface_ref`
3. `EventSource(session.events_url)`

No `ui.bootstrap` references remain.

### Current state to change

- `bundles/standard/.ai/node/routes/ui_index.yaml` still uses static
  `body_b64`.
- `static_mode` only supports inline static responses.
- `embedded_asset` is not implemented.
- `crates/clients/web/pkg/` may not exist yet; current checked-in web static
  files live under `crates/clients/web/static/` and `crates/clients/web/src/static/`.

### Files

- `crates/services/api/build.rs` or an equivalent API-side asset module
- `crates/services/api/Cargo.toml`
- `crates/services/api/src/routes/response_modes/static_mode.rs`
- optional: `crates/services/api/src/routes/static_sources/mod.rs`
- optional: `crates/services/api/src/routes/static_sources/embedded.rs`
- `bundles/standard/.ai/node/routes/ui_index.yaml`
- `bundles/standard/.ai/node/routes/ui_assets.yaml`
- `crates/clients/web/pkg/index.html`
- `crates/clients/web/pkg/bootstrap.js`
- tests, likely `crates/services/api/tests/routes_ui.rs`
- `scripts/populate-bundles.sh` if it needs to build/copy web pkg assets

### Implementation

1. Add a build-time embedded asset source for `crates/clients/web/pkg/`.
   - Prefer a small, direct implementation over a large abstraction.
   - Use `include_dir` or generated `include_bytes!` map.
   - Add `cargo:rerun-if-changed=...` if using `build.rs`.
2. Extend `static_mode` so a route can declare:
   ```yaml
   response:
     mode: static
     source: embedded_asset
     source_config:
       path: index.html
   ```
   and for assets:
   ```yaml
   source_config:
     path: "${path.asset}"
   ```
3. Serve `/ui` from embedded `index.html`.
4. Serve `/ui/assets/{asset}` from embedded assets.
5. Set headers:
   - `Content-Type` from extension (`.html`, `.js`, `.css`, `.wasm`, `.ico`)
   - `ETag` from sha256 of bytes
   - `Cache-Control: no-cache` for `index.html`
   - `Cache-Control: public, max-age=31536000, immutable` for hashed assets;
     otherwise conservative `no-cache`
   - `Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:`
   - `X-Content-Type-Options: nosniff`
   - `Referrer-Policy: same-origin`
6. Implement `If-None-Match` / ETag round trip returning `304`.
7. Write minimal `crates/clients/web/pkg/index.html` and `bootstrap.js`:
   - no `ui.bootstrap`
   - calls `/ui/api/session/current`
   - calls `items.effective` for `surface_ref`
   - opens `EventSource(events_url)`
   - renders a simple placeholder status using the returned session/surface
8. Delete the old inline `body_b64` route shape for `/ui` in the same commit.

### Tests first

- `/ui` serves embedded `index.html`.
- `/ui/assets/<asset>` serves an asset with correct content type.
- missing asset returns 404.
- ETag round trip returns 304.
- security headers are present.
- `index.html` / `bootstrap.js` contain no `ui.bootstrap` string.
- route specs compile from bundle YAML.

### Verify

```sh
cargo test -p ryeos-api --test routes_ui
cargo check --workspace
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
# verify both bundles with isolated roots, see process rule above
```

### Commit

```sh
git commit -m "slice-7: serve embedded web assets through ui routes"
```

---

## Slice 8 — web client arg propagation + descriptor completeness

### Goal

`client:ryeos/web` and the `web` binary propagate every documented launch arg
into `ui.launch.mint`. The descriptor/alias/verb surface mirrors the terminal
client where appropriate.

### Current state to change

- Binary source: `crates/clients/web/src/bin/web.rs`
- Binary target: `web`
- Current code parses only `surface`, `project`, `read_only`; it ignores parsed
  args via `_cli` and still uses placeholder token minting.
- `bundles/standard/.ai/clients/ryeos/web.yaml` lacks full arg coverage compared
  to `client:ryeos/tui`.

### Files

- `crates/clients/web/src/bin/web.rs`
- `crates/clients/web/Cargo.toml`
- tests under `crates/clients/web/tests/`
- `bundles/standard/.ai/clients/ryeos/web.yaml`
- `bundles/standard/.ai/node/aliases/web.yaml`
- `bundles/standard/.ai/node/verbs/web.yaml`
- CLI/offline-dispatch tests if alias/verb dispatch changes
- `scripts/populate-bundles.sh` only if build/install target changes again

### Implementation

1. Parse and preserve:
   - `--surface <ref>`
   - `--surface-file <path>` if the product wants parity with `tui`
   - `--mock` if the product wants parity with `tui`
   - `--read-only`
   - `--project <path>`
2. Replace placeholder UUID minting with a real request to `ui.launch.mint`.
   The request body should carry the launch context:
   ```json
   {
     "surface_ref": "surface:...",
     "project_path": "... or null",
     "read_only": true/false
   }
   ```
   If `surface_file` / `mock` cannot be represented by the current mint API,
   either extend the API in this slice with tests or explicitly remove those
   descriptor args. Do not silently parse-and-drop arguments.
3. Open the returned `launch_url` exactly as returned by the daemon.
4. Ensure `/ui/launch?token=...` shape is used consistently. Do not reintroduce
   `/ui/launch/<token>`.
5. Update `web.yaml` args to mirror the accepted binary args.
6. Ensure alias/verb dispatch for `ryeos web` reaches `client-open` for
   `client:ryeos/web` and ultimately `bin/{triple}/web`.

### Tests first

- binary request builder includes `surface`, `project`, `read_only`.
- if supported, `surface_file` and `mock` are represented or rejected clearly.
- launcher opens daemon-returned `launch_url` rather than constructing a stale
  URL shape.
- `web_descriptor_uses_bin_web`.
- `web_alias_dispatches_through_client_open`.

### Verify

```sh
cargo test -p ryeos-ui-web --bin web
cargo test -p ryeos-ui-web
cargo test -p ryeos-cli offline_dispatch
cargo check --workspace
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
# verify both bundles with isolated roots, see process rule above
```

### Commit

```sh
git commit -m "slice-8: complete web client launch args"
```

---

## Slice 9 — full workspace test and engine failure identity audit

### Goal

Run the full workspace tests and make any remaining failure state explicit by
identity, not by count.

### Tasks

1. Confirm disk headroom first. Full workspace tests can grow `target/` by ~20G.
2. Run:
   ```sh
   cargo test --workspace --no-fail-fast 2>&1 | tee .tmp/workspace-test-output.txt
   ```
3. For each failure:
   - record test name
   - expected vs. actual summary
   - whether it is pre-existing, fixed, or regressed
4. If failures are pre-existing, write:
   `.tmp/known-engine-test-failures.md`
5. Fix any regression caused by this corrective work. Do not hide failures by
   changing tests to weaker assertions.

### Verify

```sh
cargo check --workspace
cargo test --workspace --no-fail-fast
```

### Commit

If the audit produces a known-failures note only:

```sh
git commit -m "slice-9: audit workspace test failures"
```

If code fixes are needed, use a more specific `slice-9:` message.

---

## Final smoke target

After Slices 6b–8, this should work end-to-end:

```sh
ryeosd &
ryeos tui --surface surface:ryeos/cockpit/base
ryeos web --surface surface:ryeos/cockpit/base
```

Expected web flow:

1. `ryeos web` dispatches through `client:ryeos/web`.
2. `client-open` execs bundle binary `bin/{triple}/web`.
3. `web` calls `ui.launch.mint` with launch context.
4. Browser opens the daemon-returned `/ui/launch?token=...` URL.
5. Daemon sets `ryeos_session` cookie and redirects to `/ui`.
6. `/ui` serves embedded shell assets.
7. Shell calls `ui.session.current`.
8. Shell calls `items.effective` for `session.surface_ref`.
9. Shell opens `/ui/events/session/{session_id}`.
10. Actions go through `ui.actions.invoke` and publish session events.

Terminal and web renderers should share daemon semantics, effective surface
provenance, and session events. Renderer-local state such as focus/scroll stays
local.
