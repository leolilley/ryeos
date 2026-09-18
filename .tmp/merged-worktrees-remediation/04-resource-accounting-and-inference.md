# Resource settlement and resident-worker implementation: F08 / I01

Status: F08 and I01 source implementations complete and regression-tested; installed/admitted runtime qualification remains separate. Parent plan: [README](README.md). I01 is inherited, not a new ARC3 regression.

## Implemented result — 2026-09-18

F08 uses an explicit schema-v7 settlement representation and streams exact attribution pages in `(start_tick_ns, end_tick_ns, attribution_id)` order. Settlement preserves whole-operation tariff/rounding and explicit owner overhead, commits the exact page/aggregate evidence with the financial outcome, and reopens/verifies without an unbounded allocation. Attribution admission is independently bounded to indexed predecessor/successor probes rather than scanning the resident lifetime.

The v6 migration distinguishes partial advisory evidence from completed settlement: a partial record retains a null settlement timestamp and no fabricated partition, while a complete record requires its timestamp. Regression coverage performs migrate→complete→reopen/replay. Current and retained 1,025-plus-row operations, equal-start ordering, page-boundary overlap, corruption, conservation and high-cardinality admission are covered; accounting page boundaries do not terminate a resident worker.

I01 now has one request-identity-keyed inbox completion operation covering terminal publication and current-request transition. Broken terminal writes poison reuse, genuine overlap still refuses, and late cleanup/cancellation cannot clear the next request. The deterministic protocol tests and bundle authoring/model contracts pass; admitted installed model/GPU execution remains an external gate.

## F08 ownership and invariant

Files: app `accounting_db.rs`, `persistent_session.rs`, `execution_resources.rs`, runtime/reconciliation owners; executor `execution/persistent_session.rs`; accounting `resource.rs`.

Required invariant: every request released under a resource operation must have representable, conserved attribution/overhead and a terminating exact settlement path. A reporting-size limit must not strand financial holds. Process cleanup and financial settlement remain separate authoritative facts.

## Design choice and recommended staging

The primary fix is **exact bounded-memory settlement**, not accounting-driven worker rollover. The 1,024 value is a representation bound, not admitted execution policy. Retain every attribution unchanged and make settlement, startup verification, replay and readers support the same exact bounded representation. Do not simply raise the cap, drop excess rows, invent partial usage, or reset the owner.

If RyeOS independently needs a finite resident lifetime, add it as explicit signed/node policy with its own lifecycle semantics. Pooled stateless workers may be retired under that policy. A dedicated stateful session may be replaced only through its already authorized checkpoint/restore or continuation contract; absent that authority, refuse additional work without destroying session state. Do not infer replacement authority from accounting cardinality.

Before coding, inventory every reader of `ResourceUsagePartition`, its hashes/serialization, audit events and correction paths. Choose whether exact partition pages are part of the settlement transaction or a durably journaled precomputation consumed atomically by settlement. Document the choice in the implementation commit. Financial finality must not depend on an unbounded allocation or an untracked best-effort follow-up.

## Future-admission and session-continuity steps

1. Remove the mismatch between attribution admission and settlement representation. New requests remain governed by actual execution/resource policy, not the page size selected by accounting storage.
2. Preserve exact request-attempt identity across all selected resource operations. Replay must not retain attribution twice; same-input requests at distinct execution intervals must not collapse.
3. Treat unknown/interrupted request evidence conservatively. Daemon restart is not proof that a request did not execute.
4. If an independent resident-lifetime policy is introduced, define pooled retirement and dedicated checkpoint/continuation separately and test their authority. It is not part of the F08 accounting correction by default.
5. Cover single and batch attribution APIs, dedicated and pooled workers, and multi-resource requests. No partially inserted multi-resource set may become visible.

## Existing-operation settlement steps

1. Read retained attributions in chronological `(start_tick_ns, end_tick_ns, attribution_id)` order using a stable cursor over that exact tuple. Add the corresponding index/columns only through an explicit migration, or implement an equivalently bounded external ordering with exact commitments. Hash/ID order is not temporal order and cannot validate page-boundary overlap. Validate operation, request identity, coverage and non-overlap across page boundaries.
2. Preserve the existing whole-operation tariff/rounding calculation. Request shares remain floor-proportional to the total; rounding remainder is explicit owner overhead. Do not independently round each page into a new charge.
3. Prove conservation of elapsed time and allocated debit. Advisory allocated money remains zero even when a rated cost exists. Handle bounded reservation maximum, correction and late evidence under their existing semantics.
4. Persist exact page membership/order/digests and aggregate commitment if changing representation. Explicitly version new durable forms; old records retain old meanings and replay fingerprints. Bound page bytes as well as row count. Define the pagination cursor and tie behavior canonically.
5. Atomically connect settlement state, hold/debit changes, partition commitment and outbox transition. Crash before commit must be retryable without duplicate debit; append-before-ack replay must retain the same event identity.
6. Update `validate_resource_usage_partitions`, every reader and startup integrity verification in the same atomic implementation. They must stream/recompute the new representation with the same order and conservation rules; a newly settled database must reopen successfully.
7. Supply read-only diagnosis of currently stranded operations and an idempotent reconciliation path using retained evidence. Do not modify original attribution rows or clear liability merely because OS cleanup was proved.

If a bounded page representation cannot fit the current schema safely, stop the implementation at an explicit schema-design review rather than silently adding an unversioned JSON array. This is a design gate, not permission to omit existing-state recovery.

## F08 mandatory tests

Use valid sealed fixtures, not malformed observations that fail before cardinality is reached.

- 0, 1, 1,024, 1,025 and several-page disjoint requests; exact byte/page bounds.
- Bounded and advisory authority; one and multiple resources; repeated request inputs with distinct attempts.
- No accounting-page boundary unexpectedly rolls over or terminates a worker. If separately admitted resident-lifetime policy exists, test pooled and dedicated continuation semantics independently.
- Existing 1,025-row operation settles without evidence loss, financial duplication or infinite retry.
- Overlap/coverage errors straddling page boundaries; wrong operation/owner; corrupted page/commitment; count mismatch.
- Non-divisible charges/time: shares plus overhead equal exact original totals; no per-page rounding drift.
- Crash before/after partition publication, ledger commit, audit append and outbox acknowledgement; restart yields one financial outcome.
- Close and reopen the database after multi-page settlement; startup verification and all readers reproduce the same commitment without unbounded allocation.
- Attribution IDs whose lexical/hash order differs from chronological order, including equal-start tie breakers and overlap across page boundaries.
- Proved-dead owner with unresolved finance stays represented honestly until settlement completes; successful settlement permits normal owner retirement.

## I01: atomic terminal publication and inbox transition

Files: bundle `local-tinygrad/session.py`, existing protocol tests, app resident-session client/pool tests.

Required invariant: observing a terminal frame makes the peer eligible to submit the next request without the old request still causing a false-concurrency refusal. Truly concurrent requests must still be rejected.

Recommended design: make terminal publication and transition of the exact current request one synchronized inbox operation. Coordinate its lock ordering with frame writes and cancellation. A reader that receives the next request immediately after final/error must not inspect an intermediate old-current state. Do not merely clear current before sending terminal, which can admit early work or let a stale `finally` clear the next request.

Implementation tasks:

1. Introduce one completion operation keyed by exact request identity for both final and error paths. Remove unconditional duplicate clearing from `finally` once the completion operation owns it.
2. Define write-failure behavior: an incomplete terminal frame leaves delivery unknown and the channel/worker unusable; do not publish readiness for reuse or send a second terminal over a corrupted stream.
3. Preserve cancellation identity checks; specify late cancellation behavior without silently applying it to the next request. Audit lock order for reader/queue/write-lock deadlocks.
4. Keep model execution state fresh per call; this fix changes protocol sequencing, not model cache/authority semantics.

Tests: actual framing/inbox over socketpair with a scheduler barrier at terminal publication; immediate next request after final and error; true overlap before terminal; cancellation racing final; wrong-ID cancel; partial/broken write; repeated reuse; no stale cleanup clears a new request. Use a fake deterministic Worker to avoid requiring a model for the protocol regression, then rerun admitted worker tests separately.

## Acceptance and migration

Close F08 only when both new admission and retained over-limit cleanup are covered. Close I01 only with deterministic scheduling tests and an actual client reuse path. Before deploying any new accounting schema, validate upgrade against retained fixtures and document rollback compatibility; an old reader that cannot understand new partitions must not be restarted against that ledger. No attribution deletion or manual database surgery is part of the fix.
