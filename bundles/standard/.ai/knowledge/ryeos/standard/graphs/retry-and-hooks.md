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
- `retry` on a `follow: true` node is a validation error in v1. Retrying a
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
      condition: { path: status, op: eq, value: completed }
      action: { item_id: tool:ops/notify, params: { text: "graph ${graph_id} done" } }
  nodes:
    ...
```

Fire points and the context each provides:

- `graph_started` — before the walk begins (`graph_id`, `graph_run_id`, `state`,
  `inputs`).
- `graph_step_completed` — after every node, including a failed node before its
  `on_error` routing. The context carries `node`, `step`, `status` (`ok` /
  `error`), an optional `error`, and `state`, so a hook can condition on a
  node's outcome.
- `graph_completed` — at the terminal (`status`, `steps`, `success`, `state`).

Contract:

- Hooks are **observers**: a hook cannot redirect the walk — routing stays the
  walker's job. Any control value a hook returns is ignored.
- A hook action is a real dispatch on the same callback path a node action uses:
  its `effective_caps` are enforced at the callback boundary, its cost accrues to
  the run, and it is visible in the braid. A hook can only invoke items the graph
  is authorized for.
- A failing hook (its child errors, or its condition/interpolation fails) is
  recorded as a warning, not a graph failure — an observer never sinks the run.
- Hooks are fire-and-forget relative to the checkpoint: no hook state is
  persisted, and a resumed segment re-fires its hooks from the resume point
  (a bounded duplicate, consistent with the rest of crash-resume semantics).
- A hook whose `event` is none of the three fire points never runs; validation
  warns so a typo surfaces at authoring time.
