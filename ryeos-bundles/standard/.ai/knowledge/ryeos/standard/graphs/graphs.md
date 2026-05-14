---
category: ryeos/core
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

## Hooks

Hooks intercept graph events for conditional logic:

```yaml
hooks:
  - event: node_complete
    condition:
      path: "node.result.error"
      op: exists
    actions:
      - type: retry
        max_retries: 3
      - type: goto
        target: error_handler
```

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
- Cancel: `ryeos thread cancel <id>`
- Inspect state: `ryeos thread get <id>`
- Resume interrupted graphs (state persisted in CAS)
