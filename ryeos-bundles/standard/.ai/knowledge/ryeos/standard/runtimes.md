---
category: ryeos/standard
tags: [runtime, directive-runtime, graph-runtime, knowledge-runtime, llm]
version: "1.0.0"
description: >
  The three runtimes that ship with the standard bundle —
  directive-runtime, graph-runtime, and knowledge-runtime.
---

# Standard Bundle Runtimes

The standard bundle declares three runtime binaries, each serving a
different kind of workflow. They are native Linux x86_64 executables
that communicate with the daemon via the `runtime_v1` protocol.

## Directive Runtime (`runtime:directive-runtime`)

**Serves:** `directive` (default)
**Binary:** `bin/x86_64-unknown-linux-gnu/ryeos-directive-runtime`
**Required caps:** `runtime.execute`

The directive runtime handles the simplest workflow type: a single
LLM thread with a prompt + tool loop.

### How it works
1. Receives a `LaunchEnvelope` containing the composed directive
   (prompt body, context blocks, parameters, permissions)
2. Assembles the system prompt from context blocks at their declared
   positions (`system`, `user`)
3. Enters an LLM loop:
   - Sends messages + available tools to the model
   - Receives a response (text or tool call)
   - If tool call: dispatches through the daemon callback channel,
     adds result to messages, continues loop
   - If text: returns as the directive result
4. Enforces limits: `turns`, `tokens`, `spend_usd`, `duration_seconds`
5. Returns a `RuntimeResult` with the final output

### Model Selection
The runtime resolves the model from the directive's `model` config:
- `model.tier` maps to a concrete model via the routing table
- `model.name` overrides with an explicit model string
- Default tier: `general`

### Tool Dispatch
Tools declared in the directive's `actions` are presented to the LLM
as available functions. When the LLM calls one, the runtime dispatches
through the daemon's HTTP callback channel, which enforces permissions.

## Graph Runtime (`runtime:graph-runtime`)

**Serves:** `graph` (default)
**Binary:** `bin/x86_64-unknown-linux-gnu/ryeos-graph-runtime`
**Required caps:** `runtime.execute`

The graph runtime handles DAG-based workflows defined in YAML.
It delegates to the state-graph walker (in the core bundle) for
the actual graph traversal.

### How it works
1. Receives a `LaunchEnvelope` containing the graph YAML definition
2. Validates the graph (reachability, cycle detection)
3. Walks nodes according to edges and conditions
4. Persists state at each step (CAS snapshots)
5. Supports resume from persisted state after interruption

See `knowledge:ryeos/core/graphs` for the full graph YAML format.

## Knowledge Runtime (`runtime:knowledge-runtime`)

**Serves:** `knowledge` (default)
**Binary:** `bin/x86_64-unknown-linux-gnu/ryeos-knowledge-runtime`
**Required caps:** `runtime.execute`
**Status:** V5.3 stub — full functionality in V5.4

The knowledge runtime handles knowledge composition operations.
Currently a placeholder that will be fully implemented in V5.4.

### Planned Operations
- `compose` — assemble knowledge entries into a prompt context block
  within a token budget
- `compose_positions` — compose knowledge at specific prompt positions
  with per-position budgets

## Runtime Selection

When the daemon dispatches a directive or graph execution:
1. It looks up the item's kind
2. The kind schema specifies `delegate: { via: runtime_registry }`
3. The runtime registry finds a runtime that `serves` the kind
4. The daemon spawns the runtime subprocess via `runtime_v1` protocol

Each kind has exactly one default runtime. Additional runtimes for
the same kind can be registered but are not yet selected automatically.

## ABI Version

All runtimes use ABI version `v1`. The `LaunchEnvelope` and
`RuntimeResult` structures are versioned — breaking changes require
a new ABI version.
