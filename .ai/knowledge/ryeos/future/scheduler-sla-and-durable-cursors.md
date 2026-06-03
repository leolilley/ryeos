<!-- ryeos:signed:2026-06-03T04:03:57Z:3f7de18ae3945aa681e255fce88ccacb0d4cd442f82be16660bf2d1d50531379:SAg48QRgRFtb4KrBERMQ0KRNkJRCGKf0cO5D1gajiANEI2Hsdz5yNAIq4H9QM7FbP0dJcS9Yp+Yz1EsQ2TznDQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
```yaml
category: ryeos/future
name: scheduler-sla-and-durable-cursors
title: Scheduler SLA Semantics and Durable Cursor Model
entry_type: implementation_guide
version: "0.1.0"
author: amp
created_at: 2026-06-03T00:00:00Z
description: Future implementation plan for richer RyeOS scheduler semantics after the immediate v2 misfire correctness fix.
tags:
  - scheduler
  - cron
  - misfire-policy
  - durable-state
  - observability
  - distributed-scheduling
  - future-work
```

# Scheduler SLA Semantics and Durable Cursor Model

## Purpose

This note records the advanced scheduler path that should be implemented after the immediate RyeOS v2 scheduler correctness work.

The current production fix establishes the essential baseline semantics:

- live timer evaluation treats only fires before the current due boundary as misfires;
- the current due boundary is dispatched normally, even if the timer wakes a few milliseconds after the boundary;
- startup reconciliation evaluates genuinely missed fires before `now`;
- misfire catch-up intents are distinguished from recovery of already-persisted fires.

That fix is intentionally small and correct. The advanced path below should not be pulled into the current cutover unless the product needs explicit latency SLAs, high-availability scheduling, or deeper operator observability.

## Problem statement

The scheduler currently answers the basic question:

> Should this schedule fire now, and how should missed prior boundaries be handled?

Longer term, operators will need clearer answers to a richer set of questions:

- How late can a fire be before it becomes a misfire?
- What is the next planned fire time and why?
- Is the scheduler merely delayed, recovering from downtime, or contending with another owner?
- Which node owns a schedule in a multi-node deployment?
- Which fires were skipped because of overlap, lateness, disabled schedules, lost threads, or policy?
- Can the scheduler explain its decision without reading raw JSONL and SQLite projections?

The advanced implementation should turn those concepts into explicit source-controlled schema and first-class runtime state.

## Proposed model

### 1. Per-schedule lateness grace

Add an explicit grace/SLA field to schedule specs.

Example:

```yaml
schedule_id: snap-track-discover-feed-scrape
item_ref: graph:snap-track/discover_feed_scrape
schedule_type: cron
expression: "0 */15 * * * *"
timezone: UTC
misfire_policy: skip
overlap_policy: skip
lateness_grace_secs: 120
```

Semantics:

```text
scheduled_at = 03:45:00.000
now          = 03:45:00.250
grace        = 120s
decision     = normal dispatch

scheduled_at = 03:45:00.000
now          = 03:49:00.000
grace        = 120s
decision     = misfire policy
```

This separates normal scheduler jitter from meaningful lateness. The current half-open misfire window prevents the current boundary from being accidentally skipped; lateness grace would define when a delayed current fire is no longer acceptable.

Suggested defaults:

- cron: small but non-zero grace, such as 60 seconds;
- interval: grace derived from interval length, capped by a default maximum;
- one-shot `at`: default to `skip` after grace unless explicitly configured otherwise.

Do not silently invent large grace values. Make defaults visible in `scheduler show` / `scheduler explain`.

### 2. Durable schedule cursors

Persist scheduler planning state rather than deriving every operator-facing answer from fire history alone.

Suggested projection:

```text
schedule_cursors
  schedule_id
  generation
  last_fire_at
  next_fire_at
  last_evaluated_at
  owner_id
  owner_lease_expires_at
  updated_at
```

Rules:

- `next_fire_at` is advisory but persisted for introspection and fast scheduling.
- fire claiming remains the source of idempotency.
- cursor state must be rebuildable or repairable from signed specs plus fire history.
- cursor updates should be transactional with fire claim/recording where possible.

Benefits:

- `scheduler list` can show the next planned fire without recomputing in client code;
- operator debugging has a stable decision trail;
- missed-fire windows become easier to reason about;
- future distributed ownership has a natural place to store lease state.

### 3. Explicit fire lifecycle states

Keep the existing fire record but make lifecycle states more precise and consistent.

Suggested states:

```text
pending
claimed
dispatching
running
completed
failed
cancelled
skipped
misfired
```

Suggested reason/outcome taxonomy:

```text
normal
manual
startup_reconcile
recovery_thread_lost
recovery_interrupted_dispatch
overlap_policy_skip
misfire_skip
misfire_fire_once
misfire_catch_up
misfire_skipped_bounded
misfire_skipped_window
schedule_disabled
capability_denied
dispatch_failed
thread_failed
thread_cancelled
```

State machine sketch:

```text
pending
  ├─ due within grace ─────────────▶ claimed ─▶ dispatching ─▶ running ─▶ completed
  │                                                              ├──────▶ failed
  │                                                              └──────▶ cancelled
  ├─ due but overlap forbidden ───▶ skipped(overlap_policy_skip)
  └─ too late for grace ──────────▶ misfired ─▶ policy action
                                                   ├─ skipped(misfire_skip)
                                                   ├─ claimed(misfire_fire_once)
                                                   └─ claimed(misfire_catch_up)
```

The immediate v2 fix should not add all of these states, but future work should avoid stuffing semantically different cases into ambiguous `skipped` rows without a precise reason.

### 4. Scheduler ownership and leases

If RyeOS ever runs more than one scheduler-capable node for a project, schedule ownership must become explicit.

Suggested lease fields:

```text
scheduler_owner_id
lease_acquired_at
lease_expires_at
lease_epoch
```

Rules:

- only the active lease owner evaluates due fires for a schedule;
- fire IDs remain deterministic and idempotent;
- claims must still be safe if two owners race during lease transfer;
- scheduler identity should be visible in fire records and operator commands.

This should be deferred until RyeOS needs HA or multi-node hosted deployments. Do not complicate the single-node scheduler with distributed leasing prematurely.

### 5. First-class scheduler explain and diagnostics

Add operator commands that expose scheduler decisions without requiring raw SQL/JSONL inspection.

Desired commands:

```bash
ryeos scheduler list
ryeos scheduler show snap-track-discover-feed-scrape
ryeos scheduler fires snap-track-discover-feed-scrape --since 24h
ryeos scheduler explain snap-track-discover-feed-scrape
ryeos scheduler explain-fire snap-track-discover-feed-scrape@1780458300000
```

Example `scheduler explain` output:

```text
Schedule:         snap-track-discover-feed-scrape
Item:             graph:snap-track/discover_feed_scrape
Expression:       0 */15 * * * *
Timezone:         UTC
Enabled:          true
Now:              2026-06-03T04:07:00Z
Registered at:    2026-06-03T03:20:00Z
Last fire:        2026-06-03T04:00:00Z completed
Next fire:        2026-06-03T04:15:00Z
Misfire policy:   skip
Overlap policy:   skip
Lateness grace:   60s
Owner:            ryeos-node-v2@ef5c...
Decision:         waiting for next boundary
```

Example `scheduler explain-fire` output:

```text
Fire:             snap-track-discover-feed-scrape@1780458300000
Scheduled at:     2026-06-03T03:45:00Z
Observed at:      2026-06-03T03:45:00.002Z
Decision:         normal dispatch
Reason:           current due boundary within grace
Thread:           T-...
Outcome:          completed
```

These commands are the main DX improvement. They should make incidents like the Snap Track cutover diagnosable without ad hoc database queries.

## Implementation phases

### Phase 1: Schema and projection cleanup

- Add explicit normalized schedule fields for grace and policy defaults.
- Add cursor projection table.
- Add durable owner metadata fields but leave single-owner behavior unchanged.
- Keep projection rebuild deterministic from signed specs and fire history.

### Phase 2: Explain commands

- Implement read-only scheduler explain/list/fire commands.
- Prefer exposing computed decisions before changing dispatch behavior.
- Add tests with fixed UTC timestamps for boundary behavior.

### Phase 3: Grace-aware dispatch

- Apply `lateness_grace_secs` in live timer decisions.
- Keep half-open misfire windows as the baseline.
- Record late-current-boundary decisions explicitly.
- Ensure overlap and misfire policy reasons remain distinct.

### Phase 4: Distributed owner leases

- Add owner identity and lease acquisition.
- Keep fire claims idempotent and deterministic.
- Add failover tests for owner loss around a due boundary.
- Add operator visibility for owner transitions.

## Pull-forward triggers

Pull this work forward when any of the following becomes true:

- operators need to distinguish scheduler jitter from true lateness;
- production incidents require raw SQLite/JSONL inspection to explain scheduler behavior;
- RyeOS hosts multiple scheduler-capable nodes for the same project;
- schedules have materially different freshness SLAs;
- catch-up behavior is needed for more than a small number of internal jobs;
- customers/operators ask for next-fire visibility and durable schedule status.

## Non-goals for the immediate fix

Do not include the following in the current Snap Track cutover fix:

- new schedule config fields;
- distributed leases;
- persistent cursor schema migrations;
- new CLI surfaces;
- broad fire lifecycle rewrites.

The immediate fix should stay focused on correct v2 semantics: the current live due boundary must not be misclassified as a misfire, and reconcile catch-up should dispatch new misfire fires through the normal claim path rather than recovery reclaim.
