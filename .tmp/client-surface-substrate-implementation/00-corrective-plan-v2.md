# Client / Surface / Renderer Substrate — Corrective Implementation Plan (v2)

Date: 2026-05-24
Branch base: `next` (HEAD `3daa45d5`).
Mode: **slice-by-slice development directly on `next`**. No feature
branches. Each slice is a focused diff committed to `next`: tests
first, implementation, verify, commit. The working tree stays on
`next` between slices. No autonomous end-to-end execution. No
self-certified "green."

Supersedes: `.tmp/client-surface-substrate-corrective-implementation.md`.
That document was the first cut; this revision incorporates a
pressure-test pass that surfaced three structural problems before any
slice ran. Both docs share the same goals; the differences are
called out in §2.

Future-work reference: the optional descriptor-instance validator
slice is parked at
`.ai/knowledge/ryeos/future/descriptor-instance-validation.md`
(see §3). It is **not** on this corrective train.

---

## 1. Why this exists

The overnight autonomous run delivered real substrate work in Phases
1–2 (binary resolver hardening, `bundle_root`, typed errors,
`affordances` rename, `EffectiveSurface` DTO, `CommandRegistry`,
crate normalization to `crates/clients/*` plus `crates/services/ui`) but **Phases 3–5 are scaffolding
that compiles but does not function end-to-end**: route YAMLs
return placeholder HTML, `ui.bootstrap` mints fresh UUIDs and never
registers them, `/ui/launch` returns JSON instead of cookie+redirect,
`ui.actions.invoke` returns `"accepted"` without doing anything,
`ui.launch.mint` does not exist, the web launcher drops every CLI
arg via `_cli`, and `embedded_asset` is unimplemented.

We roll forward, not back. Phases 1–2 stand; phases 3–5 left useful
primitives in place (`BrowserSessionStore`, `SessionBus`,
`RouteStreamEnvelope`, `ryeos-web-launcher`) that we complete.

## 2. Structural revisions in v2

The first-cut plan was pressure-tested before execution. Three
structural revisions made it into this v2:

### Revision A — move the `client` kind out of `bundles/core`

`bundles/core` ships substrate the daemon needs to boot (tool,
service, route, knowledge, plus the engine itself). `client` is a
renderer/launcher concern. It belongs in `bundles/standard`
alongside the renderer descriptors and binaries. This also keeps
the door open to extract `ryeos web` into its own bundle later
without touching core.

Kind schemas are first-found-wins across bundle roots; the move
must be **atomic** (same commit) or duplicate-provider ambiguity
appears.

### Revision B — drop `serves` modelling from `ryeos-core-tools`

`ryeos-core-tools` ships with `bundles/core`. After Revision A,
`client` is defined in `bundles/standard`. core-tools reaching
across the bundle boundary to gate on a value (`serves.kind ==
"surface"`) whose schema it doesn't own is doubly wrong: wrong
bundle, gating on a non-behavioral field.

Nothing actually reads `client.serves` for behavior anywhere — not
the launcher binaries, not the alias/verb dispatch, not the engine.
It's descriptive metadata for registries/humans. core-tools'
`EffectiveClientDescriptor` reduces to the fields it behaviorally
needs:

- `launch.mode`
- `launch.binary_ref`
- `EffectiveItemSource.bundle_root` (already required)

The `serves.kind != "surface"` Rust branch is deleted. No
prerequisite validator slice is needed because we are not relying on
the engine to enforce the value — we are stating that core-tools
does not care.

### Revision C — replace `ui.bootstrap` with a slim session-context endpoint

`ui.bootstrap` does five things: resolves a surface (duplicates
`items.effective`), mints an untracked UUID (duplicates
`BrowserSessionStore`), returns capabilities (already in
`surface.composed_value`), returns events URL (conventional from
`session_id`), returns an empty snapshot. It is a fan-out aggregator
with no resolution logic of its own.

The browser still needs a way to discover `session_id`,
`surface_ref`, and `read_only` from its cookie. The replacement is a
slim authenticated endpoint, `ui.session.current`, that returns just
the session record. The browser flow becomes:

1. `/ui` loads shell (embedded asset).
2. shell calls `ui.session.current` — reads cookie, returns
   `{ session_id, surface_ref, project_path, read_only }`.
3. shell calls `items.effective { canonical_ref: session.surface_ref }`.
4. shell opens SSE at `/ui/events/session/{session_id}`.

Cap intersection moves to invocation time inside `ui.actions.invoke`
— not pre-baked at bootstrap.

Terminal renderer is unaffected: it is exec'd by `client-open`,
inherits stdio, calls `items.effective` for its surface directly.
No session abstraction is forced on the terminal.

## 3. Process rules (apply to every slice)

0. **No backwards compatibility. No legacy refs. No migration
   aliases. No "support both for one release" code paths. No
   deprecation shims.** When something is renamed, moved, or
   replaced, the old form is deleted in the same commit. When a
   protocol shape changes, every in-tree consumer is updated in
   the same commit. The only compatibility guarantee is "after
   the slice lands, things work." If a slice tempts you to leave
   the old code in for one cycle, that's a smell — split the
   slice instead.
1. Work stays on `next`. One slice = one focused diff. Commit
   message pattern: `slice-<id>: <short description>`.
2. **Tests first.** Write the named tests in this doc before the
   implementation. They should fail initially, then pass after the
   slice lands.
3. Each slice ends with: `cargo check --workspace` clean,
   slice-named tests green, no new warnings (or the new warnings
   recorded in the slice description), bundle-verify clean if the
   slice touches any bundle.
4. After committing the slice, re-publish bundles with
   `./scripts/populate-bundles.sh --key
   .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev` and verify both
   bundles before starting the next slice.
5. **No self-certification.** The slice description records what
   was done; the human reviewer reads the diff on `next` and
   decides green/amber/red before the next slice starts.
6. If a slice reveals a structural problem that exceeds its scope,
   commit what's clean, open a follow-up note in `.tmp/`, and stop
   — don't push scope into the next slice.

## 4. Slice ordering

The order is chosen so each slice **either tests current behavior**
or **fixes a wrong primitive before consumers come online**.
Tests-first slices come first.

| # | Slice | Why this position |
|---|-------|-------------------|
| 0 | Backfill durable invariants only | **Done** — durable tests landed before deleting bootstrap/serves behavior. |
| 1 | Revision A + drop `serves` from core-tools | **Done** — client kind moved to standard; core-tools shrank to behaviorally needed fields. |
| 2 | `items.effective` typed errors + diagnostics | **Done** — typed error codes/diagnostics are in place. |
| 3 | Session record substrate + real `ui.launch.mint` | **Done** — session records bind launch context; launch consumes token/cookie/redirect. |
| 4 | Replace `ui.bootstrap` with `ui.session.current` | **Done** — bootstrap deleted; slim session-context seam shipped. |
| 5 | Real `ui.actions.invoke` with invocation-time caps | **Done** — session/read-only enforcement and session-bus publishing landed. |
| 6a | `session_events` source + path strategy | **Done** — `/ui/events/session/{id}` uses `source: session_events`; envelope adoption/tests remain. |
| layout | Normalize crates to `clients/*` and `services/ui` | **Done** — renderer crates moved, shared client substrate renamed to `clients/base`, UI/browser substrate extracted from API/app. |
| 6b | `RouteStreamEnvelope` adoption for all streams | Remaining Slice 6 work: cut existing stream invokers over atomically and delete old SSE framing helpers. |
| 7 | `embedded_asset` source + `/ui` shell rewrite | `/ui` shell now calls `ui.session.current` → `items.effective` → SSE; no bootstrap references remain. |
| 8 | Launcher arg propagation + `client:ryeos/web` completeness | Brings `ryeos web` to a working baseline. |
| 9 | Workspace test pass + 53-failure identity audit | Final hygiene; not a feature. |

Roughly half a day each. Do not parallelize unless the dependency
graph above explicitly allows it.

---

## Slice 0 — Backfill durable invariants only

**Status:** Done in `slice-0`: durable invariants were backfilled and verified before subsequent behavior changes.

---

## Slice 1 — Revision A: move `client` kind + drop `serves` in core-tools

**Status:** Done in `slice-1`: `client` kind moved to `bundles/standard`, `serves` modeling was removed from core-tools, bundles were republished and verified.

---

## Slice 2 — `items.effective` typed errors + diagnostics

**Status:** Done in `slice-2`: `items.effective` now exposes typed errors/diagnostics used by downstream clients.

---

## Slice 3 — Session record substrate + real `ui.launch.mint`

**Status:** Done in `slice-3`: browser session records carry launch context and `ui.launch.mint` is real.

---

## Slice 4 — Replace `ui.bootstrap` with `ui.session.current`

**Status:** Done in `slice-4`: `ui.bootstrap` was deleted and replaced by `ui.session.current`.

---

## Slice 5 — Real `ui.actions.invoke` with invocation-time caps

**Status:** Done in `slice-5`: `ui.actions.invoke` enforces browser-session/read-only context and publishes session events.

---

## Slice 6a — `session_events` source + path strategy

**Status:** Done in `slice-6a`: `bundles/standard/.ai/node/routes/ui_events_session.yaml`
now uses `source: session_events`; `event_stream_mode` has the
`session_events` path strategy; the `SessionEventsInvocation` source
was introduced. This was explicitly a partial checkpoint before the
layout normalization.

**Still TODO:** envelope adoption across existing stream invokers,
Last-Event-ID replay/gap tests, and full stream test coverage are in
Slice 6b below.

---

## Slice layout — Normalize crates to `clients/*` and `services/ui`

**Status:** Done in `slice-layout`: crate layout now separates generic
substrate from client/UI substrate.

**Rule:** `crates/bin/*` is for user-PATH binaries only (`ryeos`,
`ryeosd`). Bundle-shipped renderer binaries live inside their renderer
crate as `[[bin]]` targets.

### Final inventory

- `crates/tui/core` → `crates/clients/base` (`package = "ryeos-client-base"`).
- `crates/tui/terminal` → `crates/clients/terminal` (keeps the
  `[[bin]] ryeos-tui` target inside the renderer crate).
- `crates/tui/web` → `crates/clients/web`.
- `crates/bin/web-launcher/src/main.rs` →
  `crates/clients/web/src/bin/ryeos-web-launcher.rs`; `crates/bin/web-launcher`
  was deleted; `crates/clients/web/Cargo.toml` declares the `[[bin]]` target.
- UI service handlers moved from `crates/services/api/src/handlers/ui_*` to
  `crates/services/ui/src/handlers/`:
  - `ui_launch.rs`
  - `ui_launch_mint.rs`
  - `ui_session_current.rs`
  - `ui_actions_invoke.rs`
- UI route invokers moved from `crates/services/api/src/routes/invokers/` to
  `crates/services/ui/src/invokers/`:
  - `browser_session_invocation.rs`
  - `session_events_invocation.rs`
- UI session state moved from `crates/core/app/src/` to `crates/services/ui/src/`:
  - `browser_session.rs`
  - `session_bus.rs`
- `crates/core/app/src/stream_envelope.rs` stays in `core/app` because it is
  transport-neutral substrate, not UI-coupled.
- `crates/core/app/src/ui_session.rs` holds only the minimal UI-session traits
  and DTOs needed by `AppState` to avoid a `ryeos-app` ↔ `ryeos-ui` Cargo cycle.
- `scripts/populate-bundles.sh` builds the web launcher with
  `-p ryeos-tui-web --bin ryeos-web-launcher`.

### Verify

```sh
cargo check --workspace
cargo test -p ryeos-api --tests
cargo test -p ryeos-ui
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
USER_SPACE=<tmp-user> RYEOS_SYSTEM_SPACE_DIR=<tmp-system> target/release/ryeos-core-tools bundle-verify bundles/core
USER_SPACE=<tmp-user> RYEOS_SYSTEM_SPACE_DIR=<tmp-system> target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core
```

---

## Slice 6b — `RouteStreamEnvelope` adoption for all streams

**Acceptance:** Every stream invoker emits `RouteStreamEnvelope` and
per-invoker SSE framing helpers are deleted. SSE framing happens in
`event_stream_mode` only. The new framer picks whatever
envelope-to-SSE shape maps cleanest from `RouteStreamEnvelope`;
there is **no obligation** to match old SSE bytes.

### Files

- `crates/services/ui/src/invokers/session_events_invocation.rs`
- `crates/services/api/src/routes/response_modes/event_stream_mode.rs`
- `crates/services/api/src/routes/invokers/gateway_stream_invocation.rs`
- `crates/services/api/src/routes/invokers/subscription_stream_invocation.rs`
- `crates/services/api/src/routes/invokers/stream_helpers.rs`
- `crates/services/ui/src/session_bus.rs` (publisher API + replay paths)
- `crates/services/ui/tests/session_events.rs`

### Changes

1. Cut every existing stream invoker (`gateway_stream_*`,
   `subscription_stream_*`, `dispatch_launch`, `thread_events`) over to
   producing `RouteStreamEnvelope`.
2. Delete per-invoker SSE framing helpers in the same commit. No parallel
   old/new paths.
3. `event_stream_mode` becomes the single SSE framer (`event:`, `id:`,
   `data:`, keepalive comments): `event:` = `envelope.event_type`, `id:` =
   `envelope.id`, `data:` = JSON-serialized `envelope.payload`.
4. Add/finish `Last-Event-ID` replay through the `SessionBus` ring; emit
   `snapshot_required` on gap.
5. Publish bus events from the right code paths:
   - thread upserts → `thread.upsert`
   - capability changes → `capability.changed`
   - surface reload availability → `surface.reload_available`
   - on first subscribe, seed a `snapshot` event.

### Tests

- `event_stream_mode` envelope framing (extend existing).
- `session_events_invoker_subscribes_and_yields`.
- `session_events_replay_with_last_event_id`.
- `session_events_gap_emits_snapshot_required`.
- Cut one existing invoker test over to assert envelopes in / SSE out under
  the new shape. Other invoker tests are rewritten against the new shape in
  the same commit. Old SSE-byte assertions are deleted, not preserved.

### Verify

```sh
cargo test -p ryeos-api event_stream
cargo test -p ryeos-ui --test session_events
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
USER_SPACE=<tmp-user> RYEOS_SYSTEM_SPACE_DIR=<tmp-system> target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core
```

### Risk

- Cutting every invoker over in one commit is a big diff → acceptable here
  because the alternative (parallel old/new framing code) is exactly the
  back-compat smell this plan forbids. Land the whole cut at once.
- The new SSE shape differs from previous per-invoker framers → expected and
  intentional. Any in-tree consumer is updated in the same commit.

---

## Slice 7 — `embedded_asset` + `/ui` shell rewrite

**Acceptance:** `/ui` and `/ui/assets/{asset}` serve real embedded
files from `crates/clients/web/pkg/` with correct content-type, ETag,
cache, and security headers. The `/ui` shell calls
`ui.session.current` → `items.effective` → SSE. No `body_b64`
placeholder, no `ui.bootstrap` references anywhere.

### Files

- `crates/services/api/build.rs` (new — embed
  `crates/clients/web/pkg/`)
- `crates/services/api/src/routes/static_sources/mod.rs` (new)
- `crates/services/api/src/routes/static_sources/embedded.rs` (new)
- `crates/services/api/src/routes/response_modes/static_mode.rs`
- `bundles/standard/.ai/node/routes/ui_index.yaml` (rewrite)
- `bundles/standard/.ai/node/routes/ui_assets.yaml` (new)
- `crates/services/api/tests/routes_ui.rs` (new)
- `scripts/populate-bundles.sh` (run `wasm-pack build` before
  building `ryeosd`)
- `crates/clients/web/pkg/` — minimal placeholder
  `index.html + bootstrap.js` checked in; `wasm-pack` overwrites
  at build time.

### Changes

1. `build.rs` reads `crates/clients/web/pkg/` and generates a
   `phf::Map<&str, &[u8]>` (or use `include_dir`). Cargo
   `rerun-if-changed=...`.
2. `static_mode` accepts `source: embedded_asset { path: "..." }`
   and uses the map. Serves `index.html` for `/ui` and
   `pkg/<asset>` for `/ui/assets/<asset>`.
3. Headers (per asset):
   - `Content-Type` from extension.
   - `ETag` = sha256 of bytes (computed at build, cached).
   - `Cache-Control: public, max-age=31536000, immutable` for
     hashed paths; `no-cache` for `index.html`.
   - `Content-Security-Policy: default-src 'self';
     script-src 'self' 'wasm-unsafe-eval'; style-src 'self';
     img-src 'self' data:`.
   - `X-Content-Type-Options: nosniff`.
   - `Referrer-Policy: same-origin`.
4. The placeholder `index.html` shell does:
   ```js
   fetch("/ui/api/session/current")
     .then(r => r.json())
     .then(s =>
        Promise.all([
          s,
          fetch(`/ui/api/items/effective?canonical_ref=${encodeURIComponent(s.surface_ref)}`).then(r => r.json())
        ])
     )
     .then(([s, surface]) => {
        // render placeholder; open EventSource(s.events_url)
     });
   ```
   No bootstrap reference remains anywhere in the shipped HTML.

### Tests

- `serves_index_html_from_embedded_assets`.
- `serves_asset_with_correct_content_type` (e.g. wasm →
  `application/wasm`).
- `etag_round_trip_returns_304`.
- `security_headers_present`.
- `missing_asset_returns_404`.
- `index_html_contains_no_bootstrap_reference` (grep-style guard
  against regression).

### Verify

```sh
cargo test -p ryeos-api --test routes_ui
./scripts/populate-bundles.sh ...
USER_SPACE=<tmp-user> RYEOS_SYSTEM_SPACE_DIR=<tmp-system> target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core
```

Smoke:
```sh
ryeosd &
curl -i http://.../ui
curl -i http://.../ui/assets/index.css
```

### Risk

- Build-time asset embedding inflates `ryeosd` → ship only `pkg/`
  contents; consider compression at rest if size grows.

---

## Slice 8 — Launcher arg propagation + `client:ryeos/web` completeness

**Acceptance:** `ryeos web` (alias) and direct
`ryeos-web-launcher` invocation propagate every documented arg into
the launch context; `client:ryeos/web` descriptor declares the same
arg surface as `client:ryeos/tui`. Slice 3 covered the launcher
internals; this slice closes the descriptor/alias/verb side.

### Files

- `crates/clients/web/src/bin/ryeos-web-launcher.rs` (audit; already touched
  in Slice 3 — confirm full arg coverage)
- `bundles/standard/.ai/clients/ryeos/web.yaml` (add missing
  `surface_file` / `mock` arg mappings)
- `bundles/standard/.ai/node/aliases/web.yaml`
- `bundles/standard/.ai/node/verbs/web.yaml`
- `crates/clients/web/tests/launcher.rs` (extend)

### Changes

1. The launcher consumes `surface`, `surface_file`, `mock`,
   `read_only`, `project` and sends them in the mint request's
   `LaunchContext`.
2. `web.yaml` `args:` map mirrors `tui.yaml` (`surface_file`,
   `mock`, `read_only`, `project`).
3. Alias / verb shapes mirror the `tui` pair.

### Tests

- Extend `launcher_calls_mint_endpoint_with_cli_args` (Slice 3)
  to cover every CLI arg if not already.
- `web_descriptor_serves_browser_renderer`.
- `web_alias_dispatches_through_client_open`.

### Verify

```sh
cargo test -p ryeos-tui-web --bin ryeos-web-launcher
cargo test -p ryeos-cli offline_dispatch
./scripts/populate-bundles.sh ...
USER_SPACE=<tmp-user> RYEOS_SYSTEM_SPACE_DIR=<tmp-system> target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core
```

---

## Slice 9 — Workspace test pass + engine-failure identity audit

**Acceptance:** `cargo test --workspace` runs to completion; the 53
pre-existing engine test failures are audited identity-by-identity
(not just by count). Either fix or open a known-issues note.

### Tasks

1. Get `cargo test --workspace` to complete. Disk-space note:
   `target/` grows to ~20G during a full workspace test; clean
   before running.
2. For each of the 53 engine failures: capture the test name,
   expected/actual diff, compare against the equivalent on
   baseline `a9d68fd6`. Three buckets:
   - **identical failure** — pre-existing, untouched by this
     work. Note in `.tmp/known-engine-test-failures.md`.
   - **fixed by this work** — used to fail, now passes (delete
     from the expected-reds list).
   - **regressed by this work** — used to pass / fail differently;
     bug. Fix or open follow-up.
3. Land `.tmp/known-engine-test-failures.md` listing every
   identity bucket so the next reviewer doesn't re-audit.

### Verify

```sh
cargo clean
cargo test --workspace --no-fail-fast 2>&1 | tee .tmp/workspace-test-output.txt
```

---

## Cumulative verification (after Slice 9)

```sh
# Targeted (per-slice)
cargo test -p ryeos-engine
cargo test -p ryeos-api
cargo test -p ryeos-app
cargo test -p ryeos-cli
cargo test -p ryeos-tools
cargo test -p ryeos-tui-core
cargo test -p ryeos-tui-terminal
cargo test -p ryeos-tui-web --bin ryeos-web-launcher

# Workspace
cargo check --workspace
cargo test --workspace --no-fail-fast

# Bundles
target/release/ryeos-core-tools bundle-verify bundles/core --registry-root bundles/core
USER_SPACE=<tmp-user> RYEOS_SYSTEM_SPACE_DIR=<tmp-system> target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core
```

Manual end-to-end smoke:
```sh
ryeosd &

# TUI path (already working)
ryeos tui --surface surface:ryeos/cockpit/base

# Web path (new — must round-trip via Slices 3/4/5/6/7/8)
ryeos web --surface surface:ryeos/cockpit/base
# expect browser to open at /ui, cookie set, shell calls
# ui.session.current + items.effective, session events stream live,
# palette commands invoke through actions.
```

Both renderers must show the same provenance / trust / spec.
Actions in one must appear in the other through session events.

---

## Risk register (consolidated)

| Risk | Slice | Mitigation |
|------|-------|-----------|
| Atomic kind-schema move; duplicate-provider ambiguity if split | 1 | Same-commit delete-from-core + add-to-standard. |
| `BrowserSessionStore` mint becomes remote auth bypass | 3 | Local-trust requirement; `mint_rejects_remote_caller` test. |
| New response mode `redirect_with_cookie` over-generalizes | 3 | Narrow mode; do not extend `static_mode`. |
| Bootstrap consumers exist outside the audit | 4 | Slice 4 acceptance includes "endpoint returns 404"; grep for `ui.bootstrap` references in the tree. |
| Action dispatch leaks Rye capabilities | 5 | Enforce caps in handler before dispatch; explicit test. |
| Stream invoker cutover is a big single-commit diff | 6 | Acceptable; parallel old/new framing would violate the no-back-compat rule. New SSE shape is whatever maps cleanest from the envelope; in-tree consumers updated same commit. |
| Build-time asset embedding inflates `ryeosd` | 7 | Ship only `pkg/` contents; revisit if size becomes a problem. |
| 53 engine failures hide a regression | 10 | Identity audit, not count audit. |

---

## What this plan deliberately does NOT do

- Carry any backwards-compatibility code, legacy refs, deprecation
  aliases, "support both for one release" code paths, or
  migration shims. The no-back-compat rule (process rule 0)
  applies to every slice; renames/moves/replacements delete the
  old form in the same commit, and in-tree consumers are updated
  in the same commit.
- Revert the existing Phase 1–6 commits. They stay; we roll
  forward.
- Add WebSocket. The seam exists; implementation out of scope.
- Add durable session storage. In-memory + TTL is enough.
- Add production deployment surface (TLS, domain, observability).
- Restructure the daemon process model. The existing UDS / HTTP
  binding is what we extend.
- Replace `CommandRegistry`, `EffectiveSurface`, or
  `RouteStreamEnvelope` with something different. They are
  substrate; treat as fixed.
- **Extend kind schemas to support nested enum/const constraints.**
  That work is captured at
  `.ai/knowledge/ryeos/future/descriptor-instance-validation.md`
  and is **not** required by this corrective train (Revision B
  resolved the immediate concern at the binary boundary).
- Extract `ryeos web` (or `ryeos tui`) into its own bundle. After
  Revision A the door is open; the move is a separate concern.

---

## Workflow notes

- Working tree stays on `next` throughout. Each slice: write tests,
  implement, run the slice's verify block, commit with
  `slice-<id>: ...`, re-publish bundles, mark progress in
  `.tmp/client-surface-substrate-implementation/progress.md`,
  pause for human review, then start the next slice.
- If a slice is going to take more than one working day, split it.
- If you discover a missing piece that's not in this plan, **stop
  and ask** before adding scope. Better to add a Slice N+1 than
  to let a slice grow.
- The agent (if used at all) is invoked per-slice with the slice's
  section copy-pasted as the prompt. The agent does not cross
  slice boundaries autonomously.
