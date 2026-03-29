```yaml
id: execution-graph-scheduling
title: "Execution Graph Scheduling — Pressure-Aware Realization of the Execute Graph"
description: The execution graph already exists — every execute call has a caller, every directive spawns a tree of tool calls. Making it explicit turns the executor into a scheduler. Priority is graph position. Backpressure is edge propagation. Scheduling is graph realization against finite compute. The tinygrad parallel — lazy graph, realize against hardware — at cluster scale.
category: future
tags:
  [
    scheduling,
    graph,
    execution,
    pressure,
    cluster,
    inference,
    tinygrad,
    realization,
  ]
version: "0.1.0"
status: exploratory
```

# Execution Graph Scheduling — Pressure-Aware Realization of the Execute Graph

> **Status:** Exploratory — extends [Sovereign Inference](sovereign-inference.md) and [Execution Nodes](execution-nodes.md) with graph-aware scheduling. Not scheduled for implementation.

## The Idea

RYE's execution model already produces a graph. Every `execute` call has a caller. That caller had a caller. A directive forks a thread → agent needs inference → `execute llm/complete` → routes to GPU → `model.generate()`. That's a path through a graph. But today nobody tracks it as a graph — each execute is independent, fire-and-forget.

Making the graph explicit changes everything. The executor becomes a scheduler. Priority is derived from graph structure, not declared by callers. Backpressure propagates through edges, not through polling. Scheduling is graph realization — walk the graph, find ready nodes, dispatch to hardware. The same model tinygrad uses for GPU kernels, applied to cluster-scale execution.

The result: no separate scheduler, no event queue, no control plane. The graph IS the queue. The executor IS the scheduler. You add nothing — you make the existing execution model aware of the structure it's already producing.

---

## The Graph Today (Implicit)

Every execution in RYE already forms a tree:

```
POST /execute { directive: "snap-track/add-show", username: "mrbeast" }
  └─ ExecuteTool.handle("directive", "snap-track/add-show", ...)
       └─ fork thread → agent loop
            ├─ llm/complete (turn 1) → completions server → GPU node
            ├─ snap-track/profile-scraper (local)
            ├─ llm/complete (turn 2) → completions server → GPU node
            ├─ snap-track/tile-scorer × 3 (local)
            ├─ llm/complete (turn 3) → completions server → GPU node
            ├─ snap-track/store-metrics (local)
            └─ llm/complete (turn 4) → completions server → GPU node
```

This structure exists at runtime — the call stack, the agent loop, the chain resolution. But it's not tracked. Each `execute` completes independently. Nobody can see the whole picture.

The [Execution Nodes](execution-nodes.md) design routes `llm/complete` to GPU nodes by querying `/status` and picking the least loaded. This works, but it's blind to context. It doesn't know that this `llm/complete` call is blocking an interactive user, or that it's turn 3 of 4 in a thread that's already spent 12 seconds on two GPU round-trips to the same node (whose KV cache is warm for this conversation).

---

## Making It Explicit

Every `execute` call registers as a node in a live execution graph. Edges are caller→callee. Nodes have states: **ready**, **running**, **blocked**, **complete**, **failed**.

```
graph node {
  id: execution_id
  parent: caller's execution_id (null for roots)
  tool_id: "llm/complete/meta-llama/llama-3-1-8b"
  state: ready | running | blocked | complete | failed
  root_context: { origin: "webhook", timestamp: ..., thread_id: ... }
  resource_requirements: { capability: "rye.execute.tool.llm.complete.*" }
  result: (populated on completion)
}
```

The graph is live — nodes are created when `execute` is called, transition through states as work progresses, and complete or fail. The entire execution state of the cluster is one graph.

---

## What Falls Out for Free

### Priority Is Graph Position, Not a Label

A webhook triggers `snap-track/add-show`. Someone is waiting. Every node downstream in this subtree inherits urgency from its root. The graph knows this — trace any node's path to its root and you know how urgent it is.

A cron triggers `snap-track/daily-scrape`. Nobody's waiting. Same tools, same `llm/complete` calls. But the root is different. Lower urgency, structurally.

Priority is a function of graph position:

```
effective_priority = f(
  root_context,          — webhook vs cron vs speculative
  depth,                 — how far from root (deeper = more invested)
  fan_out_blocked,       — how many downstream nodes are waiting on me
  time_in_ready_state    — aging prevents starvation
)
```

You don't tag priority. You derive it. A node blocking 12 downstream nodes is more urgent than a leaf. A node whose thread has already completed 3 of 4 turns is more urgent than one just starting — you've invested more and you're closer to done.

No caller can declare themselves "critical." The graph decides.

### Backpressure Is Edge Propagation, Not Polling

GPU node saturated → `llm/complete` nodes can't advance → they stay in **ready** state → their parent nodes (agent turns) are **blocked** → the blocking wave propagates up through edges.

You don't poll `/status`. You don't check a queue depth. The graph IS the pressure signal. At any moment you can see: "14 nodes blocked on GPU inference, 3 nodes blocked on Supabase writes." That's the shape of the graph right now. The bottleneck is visible as a cluster of blocked nodes — you can point at it.

```
                          ┌─ llm/complete [BLOCKED] ─── gpu capacity
daily-scrape [BLOCKED] ──┤
                          ├─ llm/complete [BLOCKED] ─── gpu capacity
                          └─ tile-scorer  [READY]   ─── can advance
```

The graph shows you: GPU is the bottleneck, but `tile-scorer` can advance independently. A flat queue can't express this — it would just show "3 items waiting."

### KV Cache Affinity Is Structural

Turns in an agent thread form a path in the graph:

```
thread-abc
  ├─ llm/complete (turn 1) → gpu-2
  ├─ tool execution (local)
  ├─ llm/complete (turn 2) → ???
  ...
```

Turn 2's `llm/complete` shares a path with turn 1. The graph knows they're related — same parent thread, sequential dependency. Route turn 2 to the same GPU node. The KV cache is warm.

The graph edge IS the session. No affinity key needed. The executor sees the path and makes the obvious choice.

### Cancellation Is Pruning

Directive cancelled? Prune its subtree. Every node downstream — running, ready, blocked — is cancelled in one operation. No need to track individual requests or leases.

Timeout on a node? Prune it and everything depending on it. The graph makes the blast radius explicit — you can see exactly what's affected before you cut.

```
add-show [CANCELLED]
  ├─ llm/complete [PRUNED]
  ├─ profile-scraper [PRUNED]      ← was running, killed
  └─ (remaining turns never created)
```

### Observability Is Graph Visualization

The execution graph renders directly. Bottlenecks are clusters of blocked nodes. Priority is visible as path structure. Utilization is the ratio of running to ready nodes. One diagram tells you everything a dashboard full of queue-depth metrics never could.

You can answer questions structurally:

- "Why is this directive slow?" → trace its path, find the blocked node
- "Which GPU node is the bottleneck?" → find the node that appears in the most blocked paths
- "What happens if gpu-2 goes down?" → prune all nodes routed to gpu-2, see what's affected

---

## Scheduling as Graph Realization

### The Tinygrad Parallel

tinygrad builds a lazy computation graph — tensor operations don't execute immediately, they create nodes. `.realize()` walks the graph, schedules operations to GPU devices, and executes kernels.

```
tinygrad:
  tensor ops → lazy graph → realize() → schedule to GPU devices → execute kernels

Rye:
  execute() calls → execution graph → realize() → schedule to cluster nodes → run tools
```

Same model, different scale. tinygrad doesn't have a separate "kernel scheduler" — it walks the graph, finds what's ready, dispatches to hardware. The graph IS the scheduler.

Rye does the same: the executor walks the execution graph, finds ready nodes, dispatches to cluster nodes. The difference — tinygrad's graph is static (built once, realized once), Rye's graph is live (new nodes appear as agents make decisions). But the realization model is the same.

### The Realization Loop

The executor continuously realizes the graph against available compute:

```
loop:
  1. scan graph for nodes in READY state
  2. sort by effective_priority (derived from graph position)
  3. match each ready node against available capacity
     - check resource_requirements against node capabilities
     - check utilization against pressure target
  4. dispatch highest-priority ready nodes that fit
  5. transition dispatched nodes to RUNNING
  6. on completion: transition to COMPLETE, unblock dependents
  7. on failure: transition to FAILED, propagate to dependents
```

This is the same loop the executor already runs for chain resolution — resolve what can execute, dispatch it, handle results. The change is awareness of the full graph, not just the immediate chain.

### The Pressure Target

The pressure target (e.g., 80% of compute capacity) governs how aggressively the executor realizes ready nodes:

- **Above target**: only realize high-priority ready nodes (derived from interactive/blocking graph position). Everything else waits.
- **At target**: realize normal-priority ready nodes. The cluster is working at the intended rate.
- **Below target**: realize lower-priority nodes. Start advancing speculative branches.
- **Well below target**: realize idle/speculative work to fill capacity.

The target isn't "keep GPUs at 80%." It's "keep the graph advancing at 80% of maximum throughput." The 20% headroom means an interactive request always has room — its derived priority ensures it gets realized immediately, borrowing from the headroom.

---

## Speculative Execution — Branch Prediction at Cluster Scale

The agent is about to process 50 shows. It queried the database, it has the list. Before it starts the loop, the graph extends with **tentative** nodes:

```
daily-scrape
  ├─ show-1
  │    ├─ llm/complete (speculative)
  │    ├─ profile-scraper (speculative)
  │    └─ ...
  ├─ show-2
  │    ├─ llm/complete (speculative)
  │    └─ ...
  ...
  └─ show-50
       └─ ...
```

These are speculative branches — nodes that might execute. Their effective priority is low (speculative root context). When the cluster has spare capacity below the pressure target, it starts realizing them:

- Pre-warm KV caches for likely contexts
- Pre-compute inference calls that are almost certain to happen
- Pre-load data that the tools will need

If the speculation was right, the result is already computed when the real node arrives — the agent's `llm/complete` call returns instantly from cache instead of waiting for a forward pass. If wrong, the branch is pruned. The only cost is compute that would have been idle anyway.

This is what "always use 80% of compute" actually means. Not "run a background job queue." **Extend the graph speculatively and realize branches against spare capacity.**

### What Makes Good Speculative Work

Speculative nodes must be:

- **Side-effect-free.** Inference is pure — same input, same output. Speculative tool executions that write to databases are not safe.
- **Preemptible.** If real work arrives and needs the capacity, speculative nodes yield immediately. They're the first to be evicted.
- **Cheap to validate.** When the real node arrives, checking whether speculation was correct should be a cache lookup, not a re-computation.
- **Derived from the graph.** The graph tells you what's likely to happen next — the agent has a list of 50 shows and a directive that says "for each show, scrape and score." The speculative branches are structurally predictable.

---

## Interaction with Existing Infrastructure

### What Changes

| Component                     | Change                                                                            |
| ----------------------------- | --------------------------------------------------------------------------------- |
| `ExecuteTool.handle()`        | Registers each execution as a node in the graph with parent linkage               |
| `PrimitiveExecutor.execute()` | Graph-aware realization — considers priority, pressure, affinity when dispatching |
| GPU node routing              | Replaces `/status` polling with graph-derived scheduling decisions                |
| Completions server            | Becomes an HTTP surface over graph-aware `execute` — not a queue manager          |

### What Doesn't Change

| Component               | Status                                                                                                   |
| ----------------------- | -------------------------------------------------------------------------------------------------------- |
| Chain resolution        | Unchanged — chains resolve the same way, just tracked as graph edges                                     |
| `_dispatch_remote()`    | Unchanged — still the mechanism for sending work to a remote node                                        |
| CAS sync protocol       | Unchanged — still how workspaces materialize on remote nodes                                             |
| `cas/remote.yaml`       | Unchanged — still how nodes are named and addressed                                                      |
| Ed25519 signing         | Unchanged — signed items are signed items                                                                |
| 3-tier space resolution | Unchanged — same resolution model                                                                        |
| `/status` endpoint      | Still useful for capacity reporting — but routing decisions are graph-informed, not purely status-driven |

### Completions Server Role

The completions server doesn't become a scheduler. It remains an HTTP surface — `/v1/chat/completions` — that translates incoming requests into `execute` calls. The graph-aware scheduling happens in the executor underneath. The completions server doesn't hold a queue, manage priorities, or make routing decisions. It calls `execute`, and `execute` knows the graph.

---

## How This Relates to "No Control Plane"

The execution graph is not a control plane. It's the execution state itself — the live record of what's happening right now. There's no separate controller watching the graph and making decisions. The executor walks the graph and realizes nodes as part of its normal operation.

An agent that wants to influence scheduling doesn't talk to a scheduler — it modifies the graph. Adding speculative branches, cancelling subtrees, restructuring dependencies. These are all `execute` calls. The scheduling behavior emerges from the graph shape and the realization policy.

The realization policy itself — the pressure target, the priority derivation function — can be a config or a directive. An agent can execute `cluster/policy` to change the target: "we're about to do a big batch — drop to 60% to reserve more headroom." When it's done: "raise to 90%, burn through the backlog." The agent IS the operator, and the operational lever is the graph.

---

## Open Design Questions

### Graph Persistence and Scope

How long does the graph live? Options:

- **Ephemeral**: only tracks active executions. Completed nodes are pruned. The graph is a snapshot of "right now." Simplest, lowest overhead.
- **Session-scoped**: the graph persists for the lifetime of a directive's execution tree. Completed nodes remain for observability and affinity decisions. Pruned when the root completes.
- **Durable**: the graph persists across restarts. Enables historical analysis, pattern detection for speculation, and recovery of interrupted work.

Session-scoped is likely the right starting point — you need completed nodes for KV cache affinity (knowing which GPU handled earlier turns) but don't need permanent history.

### Where the Graph Lives

With a single completions server and a small cluster, the graph can live in-process in the executor. As the cluster grows:

- **Single-node graph**: the completions server holds the graph in memory. Simple, but SPOF.
- **Replicated graph**: multiple completions servers share graph state via a backing store. Adds consistency concerns.
- **Distributed graph**: each node holds its local subgraph, with enough edge information to make local scheduling decisions. Most complex, most resilient.

Start with single-node. The graph is lightweight — it's metadata about executions, not the executions themselves. Move to a backing store when you need HA.

### Speculation Boundaries

How does the executor know what to speculate? Options:

- **Agent-declared**: the agent explicitly emits speculative branches. "I'm about to process these 50 shows." Most accurate, requires agent cooperation.
- **Pattern-derived**: the executor recognizes patterns — "this directive always calls `llm/complete` after `profile-scraper`, so pre-warm the next inference call." Requires history.
- **Directive-declared**: the directive metadata includes speculation hints. `<speculate pattern="per-item" tool="llm/complete" />`. Data-driven, no runtime inference needed.

Agent-declared is the simplest and most Rye-native — the agent extends the graph as part of its execution, using the same `execute` primitive.

### Pressure Measurement

What inputs determine "pressure" at a node?

- **Slot-based**: active executions / max concurrent. Simple, coarse.
- **Resource-based**: GPU memory utilization, KV cache occupancy, decode throughput. Accurate, model-specific.
- **Token-budget**: estimated tokens remaining for in-flight requests. Inference-aware.
- **Composite**: derived pressure score from multiple inputs.

Slot-based is the right v1. It matches what `/status` already reports (`active`, `max_concurrent`). Resource-based is a refinement when you need finer-grained scheduling.

### Preemption Mechanics

When a high-priority node needs capacity and the cluster is at target:

- **No preemption**: high-priority node waits for a slot. Simple, but headroom must be sufficient.
- **Speculative preemption**: only speculative nodes can be preempted. Safe — speculative work is explicitly disposable.
- **Priority preemption**: any lower-priority running node can be preempted. Complex — partially completed inference is wasted compute. Only makes sense if the priority gap is large and the running work can be cheaply restarted.

Speculative preemption only is the right starting point. It's the reason speculative nodes are marked preemptible — they exist to be evicted when real work arrives.

---

## The Logical Extension

[Sovereign Inference](sovereign-inference.md) says "the model is a device driver." [Execution Nodes](execution-nodes.md) says "the cluster is just more tools." This doc says **the execution graph is the scheduler**.

Three layers, same primitive:

```
execution graph (scheduling)  — which work advances, when, where
  └─ execution nodes (routing) — which node handles which capability
       └─ sovereign inference (compute) — the model is a device driver
```

All three resolve through `execute`. The graph doesn't add a new system — it makes the existing execution model aware of its own structure. The scheduler emerges from the graph the executor already produces, the same way tinygrad's scheduler emerges from the computation graph the tensor operations already produce.

The model is a device driver. The cluster is the kernel. The graph is the scheduler. You don't add infrastructure — you realize what's already there.
