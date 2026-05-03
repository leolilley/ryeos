# REPLAY_GUARD double-recording across multi-layer auth

**Status:** documented architectural crack. Not dogfood-blocking. Production code preserves the role-split semantics; only one SSE test required a pragmatic accommodation.

## The issue

When a route is guarded by **both** the global `auth_middleware` AND the per-route `rye_signed` verifier, `REPLAY_GUARD` records the request nonce in both layers. Each layer treats the second observation of the same nonce as a replay attack and rejects the request with 401.

This bites any client that legitimately authenticates a request through both layers — for example, an operator-signed POST `/execute` followed by a GET `/threads/<id>/events/stream` where both routes want the same authenticated principal.

## Where it surfaced

During the foundation-hardening wave's Cat-1 test migration, the SSE thread-events test (`sse_thread_events_e2e_live_directive_round_trip`) needed to authenticate POST `/execute` AND the SSE stream GET with the same caller key. Attempting that triggered the double-record rejection.

The pragmatic test-side accommodation:

- POST `/execute` runs unauthenticated (no `--require-auth`).
- The unauthenticated POST falls back to the daemon's own identity (`fp:<node_fp>`) as `request_principal_id`.
- The thread is created with `requested_by = fp:<node_fp>`.
- The SSE GET signs with `fixture.node` so its principal matches the implicit-daemon-identity caller.

The role-split intent of the original auth role-split fix (commit `2c4def31`) is **preserved everywhere outside this one test**. Production code still distinguishes node-as-daemon-identity vs user-as-caller-principal cleanly. The test had to use `fixture.node` as both signer and authorized subject only because the test harness can't currently exercise the multi-layer-authenticated path.

## Why production isn't blocked

A real operator running with `--require-auth` against a daemon with default per-route policies will not hit this — they sign each request once, and only one auth layer applies per route.

The crack only matters if:

- A route is guarded by both global `auth_middleware` AND per-route `rye_signed` (currently no production routes do this).
- A workflow legitimately needs the same authenticated principal across multiple routes that each enforce their own auth (currently no such workflow exists).

## What the proper fix looks like

Either:

- **Make `REPLAY_GUARD` per-layer instead of process-global.** Each auth layer records nonces in its own scope; cross-layer replay isn't recorded as such because each layer only sees the request once from its own perspective.
- **Make the global `auth_middleware` skip nonce recording when a per-route verifier is configured to handle it.** Single-source-of-truth for replay records.
- **Define a single uniform auth verifier** at one layer (probably the per-route verifier, since route-specific policy already lives there) and remove the global `auth_middleware` for routes that have explicit per-route auth.

Pick whichever aligns with how route auth is meant to evolve. The third option is the cleanest if the eventual model is "every route declares its own auth requirement explicitly" — global `auth_middleware` becomes a default-policy fallback for routes that don't declare.

## Cross-references

- Inline comment in `ryeosd/tests/sse_thread_events_e2e.rs` next to the pragmatic accommodation.
- Foundation-hardening wave Cat-1 subagent report.
