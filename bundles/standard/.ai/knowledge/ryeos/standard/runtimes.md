<!-- ryeos:signed:2026-07-16T02:18:49Z:91bcf719c89ba424fc7096b19456239ad3895e64a12929bae2505fa86e9c08df:Nlol/Lr9vYa1cI0z0E+GKVD1D/tYGpoHmBTeDqqcY1rk1jjHJ+pP1qBJW33P4KwD7lD2ZTN45PqPe5zHUjZBDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
tags: [runtime, directive-runtime, graph-runtime, knowledge-runtime, llm]
version: "1.1.0"
description: >
  The three runtimes that ship with the standard bundle —
  directive-runtime, graph-runtime, and knowledge-runtime.
---

# Standard Bundle Runtimes

The standard bundle declares three runtime binaries, each serving a different
kind of workflow. They are native Linux x86_64 executables. Directive and graph
use the ordinary `runtime` workflow wire; knowledge uses the signed
`method_runtime` wire selected by its kind schema.

Authorized runtime subprocesses are wrapped by the node's immutable sandbox
snapshot when policy is enforced; runtime/item metadata cannot enable or
weaken it. See [Execution Isolation](../core/node/execution-isolation.md).

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
It performs graph traversal natively in Rust.

### How it works
1. Receives a `LaunchEnvelope` containing the graph YAML definition
2. Validates the graph (reachability, cycle detection)
3. Walks nodes according to edges and conditions
4. Persists state at each step (CAS snapshots)
5. Supports resume from persisted state after interruption

See [Graphs](graphs/graphs.md) for the full graph YAML format.

The graph node-result cache is private in-process state scoped to one graph
execution. `cache_result` can replay a repeated ordinary action inside that
execution without rebilling it, but cache authority never crosses a restart,
resume, graph run, or filesystem boundary. Native-resume durability comes only
from `RYEOS_CHECKPOINT_DIR` and its daemon-validated checkpoint mount.

## Knowledge Runtime (`runtime:knowledge-runtime`)

**Serves:** `knowledge` (default)
**Binary:** `bin/x86_64-unknown-linux-gnu/ryeos-knowledge-runtime`
**Required caps:** `runtime.execute`
**Protocol:** schema-selected `method_runtime`

The knowledge runtime handles bounded knowledge composition operations. The
runtime registry selects this implementation binary, while the signed
`knowledge` kind schema selects the `MethodCallEnvelope`/`MethodCallResult`
wire used for both declared methods and composition launch augmentation. It is
not directly launchable through the unrelated `runtime` protocol.

### Operations
- `compose` — assemble knowledge entries into a prompt context block
  within a token budget
- `query` — search the verified knowledge corpus
- `graph` — inspect verified knowledge relationships
- `validate` — validate the verified corpus and requested roots

The daemon also invokes `compose_positions` through the private
`compose_context_positions` launch augmentation to render specific prompt
positions with per-position budgets. It is intentionally not a generically
dispatchable method in the kind schema.

## Runtime Selection

When the daemon dispatches a directive or graph execution:
1. It looks up the item's kind
2. The kind schema specifies `delegate: { via: runtime_registry }`
3. The runtime registry finds a runtime that `serves` the kind
4. The daemon spawns the runtime subprocess via `runtime` protocol

Method-bearing kinds use a parallel schema-driven path: the registry selects
the runtime binary, and `execution.method_dispatch.protocol` selects the signed
method wire. The daemon does not infer that protocol from the runtime name or
kind name.

Each kind has exactly one default runtime. Additional runtimes for
the same kind can be registered but are not yet selected automatically.

## ABI Version

All runtime declarations use binary ABI version `v1`. The signed protocol
selected for an invocation independently versions its wire:
`runtime` carries `LaunchEnvelope`/`RuntimeResult`, while
`method_runtime` carries `MethodCallEnvelope`/`MethodCallResult`. A breaking
change requires a new applicable ABI/protocol version.
