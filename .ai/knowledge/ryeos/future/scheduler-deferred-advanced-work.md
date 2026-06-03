<!-- ryeos:signed:2026-06-03T06:30:02Z:37ab2304dffbf9a391ad47c2f33fc7057cb7cf9761412081a3d56c375f2470bc:i6ZoqZDVZERBcMdvAY/pTYulvP/vqKgVdTBu0HJWFkFAnRbAQLYMU7pk5JI5Bvke/ExS65zQ37wK/Bys33yCAw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
```yaml
category: ryeos/future
name: scheduler-deferred-advanced-work
title: Scheduler Deferred Advanced Work
entry_type: deferred_implementation_plan
version: "0.1.0"
author: amp
created_at: 2026-06-03T00:00:00Z
description: Deferred scheduler work after the initial diagnostics, lateness grace, recovery durability, and advisory cursor implementation.
tags:
  - scheduler
  - future-work
  - distributed-scheduling
  - durable-state
  - lifecycle
  - observability
```

# Scheduler Deferred Advanced Work

## Status

The safe half of the scheduler SLA/durable cursor plan has been implemented:

- shared scheduler planning diagnostics;
- `scheduler.explain`;
- `completed_at` visibility in fire history;
- `lateness_grace_secs` metadata, validation, and migration;
- grace-aware current-boundary dispatch/reconciliation;
- terminal recovery `completed_at` durability;
- advisory `schedule_cursors` projection;
- fail-closed diagnostic ownership behavior.

This document records what remains intentionally deferred.

## Deferred work

### 1. Distributed scheduler owner leases

The current scheduler remains local-process oriented. It does not yet coordinate schedule ownership across multiple daemon instances.

Future implementation should define:

- owner identity format;
- lease acquisition and renewal rules;
- lease expiry behavior;
- failover semantics when an owner disappears;
- split-brain protections;
- observability for current owner and lease expiry.

The current advisory cursor table deliberately does **not** include owner lease fields. Add them only when the distributed ownership model is ready.

### 2. Authoritative cursor-driven scheduling

Current `schedule_cursors` rows are diagnostic/advisory cache state only.

They must not be treated as:

- the idempotency source;
- the dispatch authority;
- a replacement for fire claims;
- a replacement for rebuild from signed specs and JSONL fire history.

Future cursor-driven scheduling would need a stronger contract:

- explicit freshness/invalidation semantics;
- transactional cursor advancement with fire claims;
- rebuild correctness from canonical JSONL;
- stale cursor detection on `spec_hash` changes;
- crash-recovery behavior for partially advanced cursors;
- proof that cursor state cannot skip unclaimed due boundaries.

Until then, timer and reconciliation should continue to use schedule specs, `schedule_fires`, deterministic `fire_id`s, and the shared planner.

### 3. Full scheduler lifecycle/state-machine rewrite

The current implementation keeps the existing scheduler lifecycle model and string statuses/reasons.

Deferred state-machine work should clarify:

- fire lifecycle states;
- valid state transitions;
- terminal outcomes;
- recovery-specific transitions;
- overlap-specific transitions;
- misfire-specific transitions;
- how JSONL entries map to projection rows.

This should be done as an explicit lifecycle design, not as incidental cleanup inside scheduling behavior changes.

### 4. Fire-level explain endpoint

`scheduler.explain` now explains a schedule’s current planning state. A separate `scheduler.explain_fire` endpoint remains deferred.

Future `scheduler.explain_fire` could answer:

- why a specific fire exists;
- which policy created it;
- why it was skipped, failed, recovered, or dispatched;
- which thread handled it;
- whether its terminal state is durable in JSONL;
- whether it was part of normal dispatch, misfire catch-up, manual resume, or recovery.

This should be a thin diagnostic layer over existing fire history, not a new state source.

### 5. Centralized reason/outcome enums

The implementation still uses existing string reasons/outcomes such as:

- `normal`;
- `manual`;
- `overlap_policy_skip`;
- `thread_lost`;
- `thread_failed`;
- `thread_cancelled`;
- `dispatch_failed`;
- `recovery_schedule_removed`.

Future cleanup should centralize these into constants or small enums once the lifecycle model is clear.

Avoid doing this as a broad mechanical refactor unless it directly supports lifecycle validation or diagnostics.

### 6. Transactional SQLite plus JSONL atomicity

SQLite fire/cursor projection writes and JSONL append-only history are still not one atomic transaction.

Current accepted model:

- JSONL remains the durable rebuild/audit source;
- SQLite remains a rebuildable runtime projection;
- cursors remain advisory and repairable;
- cursor refresh failures are best-effort and must not alter fire mutation results.

Future work could explore stronger durability if needed:

- write-ahead intent records;
- retry queues for JSONL/SQLite divergence;
- explicit repair commands;
- startup consistency audits;
- transaction-like ordering guarantees around JSONL append and projection updates.

Only pursue this if real failure modes require it. Do not make advisory cursors authoritative to compensate for projection divergence.

### 7. Advanced cursor performance optimization

The current cursor fields are useful diagnostics, not a planner bypass.

Future optimization may read cursors to reduce repeated planner work, but only after:

- cursor freshness is proven;
- missing/stale cursor repair is safe;
- schedule update invalidation is complete;
- projection rebuild is tested with stale fire/spec data;
- timer behavior remains correct when cursors are missing or stale.

The first optimized mode should still fall back to the shared planner and deterministic fire claim behavior.

### 8. Richer scheduler observability

Diagnostics now expose current planning state, grace, and advisory cursor fields. More operator visibility remains possible:

- schedule health summaries;
- recurring lateness metrics;
- misfire counts by policy;
- overlap skip counts;
- recovery counts;
- cursor staleness warnings;
- owner lease state once distributed scheduling exists;
- projection rebuild reports.

Keep observability read-only unless a repair action is explicitly requested.

## Guardrails for future implementation

- JSONL fire history remains canonical rebuild/audit state.
- SQLite projections remain rebuildable.
- Fire claims remain the idempotency mechanism while the DB is retained.
- Advisory cursors must never suppress or create dispatch by themselves.
- Non-admin diagnostic APIs must remain ownership-scoped.
- Anonymous or unverified scheduler diagnostics should fail closed.
- Schema migrations must preserve exact SQLite schema validation.
- Schedule updates must preserve original requester and granted capabilities unless reauthorization is explicit.

## Suggested next implementation order

1. Design the explicit fire lifecycle/state machine.
2. Centralize reason/outcome constants as part of that lifecycle design.
3. Add `scheduler.explain_fire` using existing durable history.
4. Add richer read-only observability and consistency checks.
5. Design distributed owner leases separately.
6. Only then consider authoritative cursor-driven scheduling.
