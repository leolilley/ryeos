<!-- ryeos:signed:2026-07-13T04:02:46Z:d999e51f149b70ac5945c85051d60cb99da38c4e982ecb6c668783993d20fd2e:jhwsFPcZgA0VJNYQm4cMbqKCZ5xEN6t0stULprMgBaf/BOP1cdr3JKfxjeVU9eXdXCcJtRE+1XRgYNpZ4K7BCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [reference, graphs, dag, state-machine]
version: "1.0.0"
description: >
  How state graphs work — YAML DAG definitions, node types,
  conditional edges, foreach, hooks, and state persistence.
---

# State Graphs

State graphs are declarative YAML state machines executed by the
state-graph runtime. They define multi-step workflows with
conditional branching, parallel execution, and persistent state.

## Graph Structure

```yaml
name: my-pipeline
version: "1.0.0"
description: A multi-step workflow

nodes:
  start:
    action:
      item_id: "tool:ryeos/core/fetch"
      params:
        item_ref: "knowledge:project/context"
    edges:
      - to: process
        when: "result.status == 'ok'"
      - to: error
        when: "result.status == 'error'"

  process:
    action:
      item_id: "tool:my/process"
      params:
        input: "${start.result.data}"
    edges:
      - to: finish

  error:
    action:
      item_id: "tool:my/error-handler"
    edges:
      - to: finish

  finish:
    action:
      item_id: "tool:ryeos/core/fetch"
```

## Nodes

Each node has:
- **`action`** — what to execute (tool, directive, fetch, sign)
- **`edges`** — where to go next (conditional or unconditional)
- **`cache`** — opt-in result caching
- **`error_mode`** — `fail` (default) or `continue`

### Action Types

```yaml
action:
  item_id: "tool:my/deploy"          # Execute a tool
  params: { target: "staging" }

action:
  item_id: "directive:my/review"     # Execute a directive
  params: { scope: "full" }

action:
  via: fetch                          # Fetch an item
  item_ref: "knowledge:project/api"

action:
  via: sign                           # Sign an item
  item_ref: "tool:my/helper"
```

### Edge Conditions

Edges can have `when` conditions evaluated against the current state:

```yaml
edges:
  - to: deploy
    when:
      all:
        - path: "build.result.status"
          op: eq
          value: "success"
        - path: "tests.result.passed"
          op: eq
          value: true
  - to: notify
    when:
      any:
        - path: "build.result.status"
          op: eq
          value: "failed"
  - to: default_node                  # unconditional (no when)
```

Supported operators: `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `in`,
`contains`, `regex`, `exists`.

#### Same-node conditions: read `result.*`, not `state.*`

A node's `assign` merges into state **after** its edges are evaluated, so a
`when` on `state.<key>` where the *same node* assigns `<key>` compares against
the value from before this node ran — unset on the first visit, one iteration
stale inside a loop. Branch on the node's own outcome with `result.<key>`
(the fresh result is placed in the condition context):

```yaml
recall:
  action: { item_id: "tool:recall" }
  assign: { found: "${result.found}" }
  next:
    type: conditional
    branches:
      - when: { path: "result.found", op: eq, value: "yes" }   # this node's outcome
        to: warm
      - to: study                                              # default
```

`state.<key>` is still correct for reading a value a **prior** node committed.
Graph validation warns at signing time when a node assigns `K` and a same-node
branch condition reads `state.K`.

## Foreach

Iterate over lists with parallel or sequential execution:

```yaml
nodes:
  deploy-all:
    foreach:
      over: "${inputs.targets}"
      mode: parallel          # or "sequential"
      max_concurrency: 5
      action:
        item_id: "tool:my/deploy"
        params:
          target: "${foreach.item}"
    edges:
      - to: finish
```

For managed-runtime children whose whole continuation chains must finish before
the graph proceeds, use an **action node with `follow: true` and `over`**, not a
`foreach` node:

```yaml
nodes:
  review-all:
    type: action
    over: "${inputs.subjects}"
    as: subject
    parallel: true
    max_concurrency: 4
    follow: true
    action:
      item_id: "directive:example/review"
      params:
        subject: "${subject}"
        run_id: "${_run.graph_run_id}"
    facets: {cohort: "${_run.graph_run_id}", subject: "${subject}"}
    collect: reviews
    on_error: handle-partial
    next: {type: unconditional, to: finish}
```

This cohort form requires `as` and `parallel: true`; `collect` must differ from
`as`, and `retry`, caching, and `detach` are invalid. `max_concurrency`, when
set, must be positive and bounds launched-and-live child chains. Collection is
input-ordered, failed slots are `null`, and successful assignment/collection is
committed before `on_error` routing. An empty input succeeds with `[]`. Actions,
params, and facets interpolate per item, including `${_run.graph_run_id}`. The
parent's effective capabilities and hard limits bound every child. See
`graphs/follow.md` for capability wildcard examples, cancellation/resume
behavior, and a complete authoring example.

## Hooks

Declare `config.hooks` to observe graph lifecycle events with the same typed
definition the directive runtime uses (`id`, `event`, optional `condition`,
`action`) — one hook grammar across runtimes.

```yaml
config:
  hooks:
    - id: announce_done
      event: graph_completed
      condition: { path: status, op: eq, value: completed }
      action: { item_id: tool:ops/notify, params: { text: "graph ${graph_id} done" } }
```

Fire points are `graph_started`, `graph_step_completed` (after every node,
including a failed node before its `on_error` routing), and `graph_completed`.
Hooks are **observers**: a hook action is a real dispatch (its `effective_caps`
are enforced, its cost accrues to the run, it shows in the braid) but it cannot
redirect the walk — routing stays the walker's job, and a failing hook is
recorded as a warning, never a graph failure. Node-level resilience is the node
`retry:` block, not a hook action. See `retry-and-hooks.md` for the full
contract.

## State Persistence

The state-graph runtime persists:
- **Execution snapshots** — current node, accumulated state
- **State snapshots** — variable bindings
- **Transcripts** — JSONL event log
- **Knowledge render** — signed markdown with visual status table

State is stored in the CAS, enabling resume after interruption.

## Permissions

Graph permissions are lifted by the `graph-permissions` composer
into `policy_facts.effective_caps`. Each node action is checked
against these capabilities before execution.

## Thread Integration

Graphs run as threads. You can:
- Tail events: `ryeos thread tail <id>`
- Cancel: `ryeos commands submit <id> cancel`
- Inspect state: `ryeos thread get <id>`
- Resume interrupted graphs (state persisted in CAS)
