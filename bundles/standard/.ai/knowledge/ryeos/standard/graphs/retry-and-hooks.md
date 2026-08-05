<!-- ryeos:signed:2026-08-05T08:21:19Z:23bbe6edf52a0926dfc765674697a99bf700718306a4a9fe553b72f51f80dc10:QzDnsQuemROlh0k1D/HdqVdVP/YLMhHn/7JzuCuHXNUa39zeXrCaIhvgegf1lQ169DEnpE4qr6NgqJf+XYUdCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/graphs
tags: [graph, authoring, retry, hooks, resilience]
version: "1.0.0"
description: Per-step retry and observer hooks for graph workflows.
---

# Graph retry and hooks

Two resilience/observability features on the graph runtime, both opt-in.

## Per-step retry

Add an optional `retry` block to an action node (a plain action or a
`foreach`). When a dispatch fails, the walker re-runs it up to a bounded number
of attempts with an exponential backoff before falling through to the node's
existing `on_error` routing.

```yaml
nodes:
  fetch:
    action: { item_id: tool:web/fetch, params: { url: "${inputs.url}" } }
    retry:
      attempts: 3          # TOTAL dispatches incl. the first; 1..=10
      backoff_ms: 1000     # exponential: backoff_ms * 2^(attempt-1)
      max_backoff_ms: 30000 # optional cap on the computed backoff
    on_error: handle_failure # applies only AFTER retries are exhausted
```

Semantics:

- `attempts` is the total count including the first dispatch, so `attempts: 3`
  is one initial call plus up to two retries. Exhaustion routes through the
  node's `on_error` (or the graph-level `on_error` policy) unchanged.
- `backoff_ms` and `max_backoff_ms` are each capped at 300,000 milliseconds
  (five minutes). Larger authored values are rejected during graph validation;
  runtime delay calculation also applies the same defensive cap.
- Every attempt consumes a walker step, so `max_steps` and `segment_steps`
  bound the total retry work — a retry loop can never run unbounded.
- The attempt counter is checkpointed. A segment cut or a crash mid-retry
  resumes with the count intact rather than restarting the attempts, so a
  three-attempt policy stays three attempts across the whole run.
- Each re-attempt emits a braid-visible `graph_node_retry` event carrying the
  attempt number, the total, the backoff delay, and the failure summary.
- **Cost multiplies.** Each attempt is a fresh child dispatch that accrues its
  own cost, so `attempts: 3` can triple a node's spend on a persistently failing
  child. Keep `attempts` small and reserve retry for genuinely transient
  failures (a flaky network fetch), not deterministic ones (a bad prompt).
- Only successful dispatches cache; a retried-then-successful node caches
  normally, and a failure is never cached.
- `retry` on a `follow: true` node is a validation error. Retrying a
  follow needs a fresh follow lifecycle per attempt; route a failed follow with
  `on_error` instead.
- Cancellation during a backoff is immediate — cancelling the graph kills the
  sleeping walker.

For `foreach`, `retry` applies per item-dispatch inside the single foreach
step; each item keeps its own attempt count and per-item backoff.

## Observer hooks

Declare `config.hooks` to run an action at graph lifecycle events. Hooks use the
same typed definition directives use (`id`, `event`, optional `condition`,
`action`), so one hook grammar spans the runtimes.

```yaml
config:
  start: fetch
  hooks:
    - id: announce_done
      event: graph_completed
      result: discard
      condition: 'status == "completed"'
      action: { item_id: tool:ops/notify, params: { text: "graph ${graph_id} done" } }
  nodes:
    ...
```

Fire points and the context each provides:

- `graph_started` — once, before the logical graph run begins (`event`,
  `graph_id`, `graph_run_id`, `state`, `inputs`). Continuation/resume segments
  do not re-fire it.
- `graph_step_completed` — after every node, including a failed node before its
  `on_error` routing. The context carries `event`, `graph_id`, `graph_run_id`,
  `node`, `step`, typed `status` (`ok`, `error`, or `retry`), an optional
  `error`, and `state`, so a hook can condition on a node's outcome. A retry
  completion fires before the checkpointed re-attempt advances.
- `graph_completed` — during terminal settlement (`event`, `graph_id`,
  `graph_run_id`, candidate `status`, `settled: false`, `steps`, candidate
  `success`, `state`, `inputs`). Hook cost is admitted before the authoritative
  terminal status is emitted, so an accounting-integrity failure can still
  change the settled result to `error`.

Contract:

- `result` is required. Use `discard` for an observer whose leaf value carries
  no meaning, or `observation` when the leaf publishes a bounded namespaced
  `{kind, payload}` evidence record. Graph routing never consumes hook control,
  so `result: control` is rejected before spawn in every graph hook layer.
- Hooks are **observers**: a hook cannot redirect the walk — routing stays the
  walker's job.
- A hook action is a real dispatch on the same callback path a node action uses:
  its captured source grants are enforced at the callback boundary, its cost
  accrues to the run, and it is visible in the braid. Authored hooks use the
  graph's admitted effective caps. Configured hooks use only the grants declared
  by their own signed policy source; they cannot borrow graph authority.
- An ordinary hook child error or condition/action evaluation failure is
  recorded as a routing-inert warning, not a graph failure.
- A callback can return a structurally valid dispatch envelope whose
  `result: observation` value violates the observation schema. The daemon
  records that outcome durably as routing-inert `hook_failed` evidence with
  failure class `observation_invalid`; independently, the runtime normalizes
  the same value and classifies the malformed evidence as an integrity
  failure. The event cannot steer routing, but the integrity failure
  invalidates terminal authority and therefore prevents successful
  settlement.
- Hook cost is checked, attributed by lifecycle event, included in the graph
  rollup, and persisted in advancing checkpoints. Accounting failures and
  integrity-typed callback/child failures invalidate terminal authority and
  fail the graph rather than allowing a contradictory or under-reported
  successful settlement.
- Advancing-step hooks may be re-fired after a crash before their checkpoint
  fence completes, consistent with the node dispatch fence (defined in
  `../runtimes/graph-runtime.md`: effects before the checkpoint are at-least-once
  observability, and only the checkpoint advances resumable state). Segment
  resumes do not re-fire `graph_started`, and terminal hooks are not dispatched
  after run accounting has already become invalid.
- A hook whose `event` is none of the three fire points fails graph loading.
  Unknown event names never survive into execution as inert configuration.

### Capture and inheritance

Authored `config.hooks` follows the graph's shallow config rule: omitting the
key inherits the nearest complete list, `hooks: []` clears it, and declaring a
list replaces it atomically. Hooks are not merged by ID. Mandatory policy lives
in separately signed configured layers and cannot be cleared by authored
content.

Every hook-capable launch captures one `ryeos.hooks.effective.v1` plan after
composition and before any callback token, capsule, or runtime exists. It
contains authored, builtin, infrastructure, context, operator, and project
layers plus their exact event contracts, source evidence, and per-source
dispatch grants. The runtime compiles this admitted plan and never reloads hook
policy from disk.

Configured hooks declare an explicit target pair:

```yaml
hooks:
  - id: observe_graph
    target: {kind: graph, event: graph_step_completed}
    result: observation
    action: {item_id: tool:ops/record-step}
```

Unknown kind/event pairs, duplicate effective IDs, invalid result modes,
malformed grants, wrong source space/trust, or a captured-plan/composed-hook
mismatch fail admission. Infrastructure hooks are observer-only. A hook
callback must match the exact captured owner kind, event, ID, layer, result
mode, context contract, root raw-content digest, and effective-definition
digest before the idempotency ledger is consulted.

The ledger distinguishes a known post-reservation dispatch failure from a
crash-ambiguous outcome. A returned dispatcher error or malformed callback
response is completed with a canonical integrity-failure response, replays
byte-identically, and yields `hook_failed` evidence for observation hooks. A
process loss after reservation with no known result remains pending forever:
it cannot be retried without violating at-most-once execution and therefore
fails closed as an unknown outcome. A ledger row may be deleted only after its
chain can no longer resume.
