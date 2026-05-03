# Real-install smoke test

**Status:** deferred until after first dogfood walk-through.

## What it is

A slow integration test that exercises the operator install path end-to-end:

1. Real `rye init` (no fast fixture; generates fresh keys; lays down core).
2. Real `rye vault put MOCK_API_KEY=... --principal user` (or whatever the v1 vault put UX is).
3. Start `ryeosd` with a **scrubbed parent env** (poison `OPENAI_API_KEY=POISON` first to surface inheritance bugs).
4. Plant signed `model_providers/mock.yaml` + `model_routing.yaml` + one directive (signed by the user key from step 1).
5. POST `/execute` for the directive.
6. Assert: provider received the request; auth header value came from the vault (matched `MOCK_API_KEY`); thread completed; result returned to client; `OPENAI_API_KEY=POISON` did NOT reach the provider.

Plus a second test: fresh install + invoke `tool:rye/core/verify` with **no `RYE_SYSTEM_SPACE` exported** — catches root-discovery regressions in `rye-inspect`'s engine boot path.

## Why deferred

The operator dogfood walk-through serves as the manual smoke. Anything this test would catch will be caught by the human walking through the install + first-directive run. Time-to-dogfood is the priority right now.

## When to land

After the first dogfood walk-through proves the path is sane. The test then becomes a regression net for future changes to:

- Bootstrap / `rye init` flow.
- Subprocess env scrubbing (Part B of the foundation-hardening wave).
- Manifest verification (Part C).
- Vault read path.
- Provider config loading + secret injection.
- Dispatch → runtime → SSE chain.

## File location

`ryeosd/tests/real_install_smoke.rs`. Stays on the SLOW path (alongside `cleanup_e2e.rs` and `dispatch_pin.rs`) — not a fast-fixture test. The whole point is to exercise the real init flow.

## Implementation sketch

See `.tmp/FOUNDATION-HARDENING-WAVE.md` Part F for the original spec. ~1–3 hours to write properly.

## Cross-references

- `.tmp/FOUNDATION-HARDENING-WAVE.md` Part F.
