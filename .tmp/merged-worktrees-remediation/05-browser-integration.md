# Browser/shared-model implementation: F09–F12 and design alignment

Status: F09–F12 source implementation, durable seat contract and generated browser assets complete; pinned-toolchain and installed signed-entry qualification remain open. Parent plan: [README](README.md).

## Implemented result — 2026-09-18

- The WASM boundary emits ordinary JavaScript records/arrays/null and BigInt for exact integers. The single lossless JSON encoder rejects Map/non-record values, prototype-sensitive ambiguity and unpaired surrogates; HTTP and all SSE decode paths preserve integers beyond JavaScript's safe range.
- Seat append is a durable producer/operation/sequence/digest protocol. The daemon atomically stores events and the receipt, exact replay returns the same acknowledgement, contradictions/gaps/stale producers refuse, and browser restart restores the bounded pending operation. Closing/replacing a session and 401/403 reconciliation cannot let a stale response schedule work for the new seat.
- Shared Rust semantics now address overlays, notices, group tabs, row/table/timeline choices and folds by stable instance/item/section identity. Pointer selection and optional activation are one ordered event; stale or hidden targets are no-ops. Disabled reasons and focus-return keys reach the browser accessibility layer.
- The shell renders no navigation column when none is authored. Overlays/notices are shared-model projections rather than browser-owned command policy.
- Generated TypeScript contracts, renderer assets, packaged WASM and the daemon asset registry were regenerated as one closure. Current-host Svelte, Node, populated-WASM and real headless interaction tests pass; see plan 07.

The expanded signed view inventory is also validated through the real parser/resolver/composer pipeline. All 45 views receive behavior goldens. Two authored defects were corrected rather than blessed: `programs/list` no longer uses the nonexistent view-level `chrome: thin` exception, and `thread/conversation` explicitly declares its timeline widget. The signed view kind contract retains typed optional `extends` provenance while bindings consume composed effective values.

Owners: `crates/clients/web/src/wasm.rs`; `browser/runtime/{effects,transport,session,boot}.ts`; Svelte root/layout/view components; generated contracts/exporter; shared `crates/clients/base/src/ui`; daemon seat/invocation schemas; signed UI content and asset publication.

## F09: one explicit WASM-to-HTTP contract

There are two boundaries: Rust↔JavaScript semantic values and browser↔daemon JSON. Fix both intentionally; a TypeScript assertion does not convert a runtime Map or BigInt.

1. Add populated packaged-WASM contract fixtures containing a seat facet, nested dynamic source parameters, selection record, arrays, nulls and large integers. Capture actual request bytes and deserialize them with the daemon's real request types.
2. Preferred object strategy: configure the central Rust serializer to emit the string-keyed object shape declared by generated browser types where appropriate. Audit every export, including flattened seat enums and arbitrary JSON payloads. Reject unsupported key/value shapes rather than accepting accidental `{}`. Regenerate the typed contract from the actual wire representation.
3. Retain exact BigInt semantics for core u64 identities. At HTTP encoding, define schema-aware conversion. Convert to Number only where a checked safe range is part of the endpoint contract. If the endpoint requires the full integer range, use a reviewed lossless numeric JSON encoding or explicitly version both ends; do not globally stringify BigInts into quoted decimals or truncate to Number.
4. Encode once before calculating byte limits and sending. The validated bytes must be the transmitted bytes. Reject malformed values locally before fetch, distinguishing known non-delivery from a network outcome that is genuinely unknown.
5. Do not maintain separate subtly different encoders in seat sync and invocation dispatch. Test malformed/unsupported values, special object keys, nested maps/objects and prototype-sensitive keys; only admit the intended JSON data domain.

### Seat retry and shutdown semantics

Replace recursive unconditional retry with an explicit single-flight state machine:

- Snapshot a bounded pending batch; advance acknowledged position only after confirmed success for that batch.
- New events during flight remain queued. Closing/replacing a session cancels timers and prevents stale responses scheduling work for the new session.
- Serialization/validation and permanent 4xx refusal: stop automatic retry, expose an actionable transport/notice state. Never pretend events were saved.
- Expired session/binding: invoke the existing renewal/reconciliation contract, not an invented new principal or seat.
- Retryable network/5xx outcomes: retry only one durable append operation identity. Extend the server contract to bind `(seat, producer incarnation, operation ID, exact sequence interval, canonical payload digest)` atomically to its acknowledgement. Exact replay returns that acknowledgement; contradictory reuse, gaps, stale producers and unauthorized seats refuse. Browser restart restores or explicitly reconciles the same pending identity. Until this server contract exists, unknown delivery stops automatic retry and remains visibly unresolved.
- Bound retained queue/request bytes. If a bound is reached, surface unsaved state and apply an explicit shared policy; never silently discard seat history.

Tests include append success, exact replay, contradictory replay, gaps/stale producer, permanent 400, 401/403, 5xx, lost response after accepted append, malformed response, browser restart with pending operation, concurrent new events/clients, session close/replacement, and repeated retries without duplicates or tight loops.

## F10: render shared overlays and notices

Build components against existing overlay/notice view models and shared events. Include launcher/views, commands, help/shortcuts, filtering, selection, disabled/reason states and dismiss behavior. Do not create a separate browser command registry or infer a grant from enabled styling.

Reconcile modal focus with DOM presentation: initial focus, keyboard navigation, escape, focus containment where appropriate, focus return and screen-reader labels/live announcements. Native text input/IME must not cause duplicate shared dispatch. Multiple overlay/notices must have explicit shared ordering and stable identity.

Render refusal, unknown-delivery and draft-retarget feedback visibly. Inspect whether existing model dismissal/expiry events suffice; add shared semantics if needed rather than browser-owned timers that diverge from terminal behavior. Never render untrusted notice/row content as raw HTML.

Tests use real core envelopes and browser focus/accessibility checks, not screenshots alone: open/filter/choose/escape, disabled reason, command failure, unknown outcome, draft warning, focus return after layout replacement, reduced motion and narrow viewport.

## F11: distinguish workspace and view-group selection

Replace group-local `switch_tab(index)` dispatch with the existing exact tile/group selection semantics. Verify whether `FocusChanged` fully selects the group's active member in the current reducer; if not, add a shared event with exact group/member identity. Do not add a browser-only active-tab cell.

Keep workspace strip switching separate. Tests: two workspaces with multiple groups, second member selection stays in its workspace, one-workspace case works, group reorder preserves identity, stale/removed member is safely refused, correct source refresh and draft retention.

## F12: pointer and keyboard share focus/selection

Wire tile/dock focus and row cursor updates using exact shared IDs and cursor semantics. Establish deterministic event ordering: focus target, select row, then activate only if the user requested activation and a valid intent exists. Clicking an inert record may select it without granting an action.

Audit rows, tables, timelines, sections, fields, docks and blank/header areas. Selection indices must match the shared projected ordering, including collapsed section headers; do not derive one universal DOM index. Dock identity must use its actual shared address rather than stringifying an unrelated instance key.

Prevent nested button bubbling from causing duplicate activation or overwriting explicit composer/field focus. Handle keyboard-native activation, defaultPrevented, IME and pointer focus consistently. Keep close/move/fold/contextual actions targeted to the shared selection. Test mixed pointer/keyboard flows, layout changes, removed rows and multiple identical view instances.

## Design gap and test debt

Restore launcher-led tiled workspace composition. Remove unconditional Explorer-column reservation when no authored/presentation contract calls for it. Derive optional navigation/dock presentation from the existing signed/shared model; if no such contract exists, propose it in shared content/model rather than hardcoding product navigation. Preserve authored labels, empty/error states, ambient/reduced-motion behavior and independent drafts. Validate populated arrangements at wide and narrow sizes.

Address the six failed shared-client tests in plan 06 without weakening schemas. In particular, distinguish fixture drift from the unresolved field-convergence behavior. Tests must prove valid current contracts and intended rejection, not simply update expected output to whatever happens today.

## Publication and acceptance

Use the pinned Node/npm/Playwright and retained browser assets. Run Svelte type checks, Rust/shared tests, populated packaged-WASM tests, actual browser interactions and daemon request-schema tests. Rebuild generated exports/WASM/renderer through existing build/publication tooling; refresh the complete signed asset closure/registry, not individual files copied into `pkg`.

Include installed signed-entry boot and cache-consistent asset loading in integration. Keep F09 runtime encoding, F10 presentation, F11 group selection and F12 focus closure evidence separate. Passing an empty WASM envelope or a visual mockup does not close them.
