```yaml
id: execution-nodes
title: "Execution Nodes — Distributed Computing Through RYE's Execution Model"
description: Distributed tool execution across self-reporting nodes — capabilities derived from the workspace, routing through existing execute and remote dispatch, no inference-specific logic
category: future
tags: [nodes, execution, remote, cluster, distributed, compute, routing]
version: "0.1.0"
status: exploratory
```

# Execution Nodes — Distributed Computing Through RYE's Execution Model

> **Status:** Exploratory — builds directly on existing remote execution infrastructure. Not scheduled for implementation.

## The Idea

RYE already has remote execution. `ExecuteTool._dispatch_remote()` pushes CAS objects to a named remote, triggers execution via the `rye/core/remote/remote` tool, and folds results back. The remote server (`ryeos-remote`) materializes a workspace, runs the executor, and returns.

Today this is one user pointing at one remote on Modal. This proposal extends it to multiple nodes — each running `ryeos-remote`, each self-reporting what it can do, with routing that matches workload requirements to node capabilities. The execution protocol stays the same because it's already the right shape.

The key principle: **there is no special infrastructure for specific workload types.** Inference, scraping, data processing — these are all just tool executions. A node that serves a model has a tool that calls tinygrad's `model.generate()` in-process. A node that runs a browser has a tool that launches headless Chrome. The routing system doesn't know or care what the tools do. It matches capabilities and dispatches.

---

## What Exists Today

| Component                        | Location                         | What It Does                                                              |
| -------------------------------- | -------------------------------- | ------------------------------------------------------------------------- |
| `ExecuteTool._dispatch_remote()` | `ryeos/rye/tools/execute.py`     | Routes execution to a named remote via the remote tool                    |
| `_parse_target()`                | `ryeos/rye/tools/execute.py`     | Parses `"remote:gpu"` → `("remote", "gpu")` — named remote syntax         |
| `rye/core/remote/remote`         | Core bundle tool                 | CAS sync + HTTP POST to the remote server                                 |
| `remote_config.py`               | Core bundle                      | Resolves named remotes from `cas/remote.yaml` via 3-tier resolution       |
| `ryeos-remote` server            | `services/ryeos-remote/`         | FastAPI server: `/execute`, `/push`, CAS sync, threads, webhooks, secrets |
| `modal_app.py`                   | `services/ryeos-remote/`         | Modal deployment: volume-backed CAS, scheduled execution                  |
| `cas/remote.yaml`                | `.ai/config/cas/`                | Named remotes with URL + key_env, 3-tier resolved                         |
| CAS sync protocol                | `rye/cas/sync.py`                | `has/put/get` object exchange                                             |
| `create_execution_space()`       | `rye/cas/checkout.py`            | Materializes `.ai/` workspace from snapshot on the remote                 |
| `three_way_merge()`              | `rye/cas/merge.py`               | Merges remote execution results back into project HEAD                    |
| TOFU key pinning                 | `/public-key` endpoint           | Pin remote's Ed25519 key on first contact                                 |
| Signing key                      | `SIGNING_KEY_DIR` on each remote | Ed25519 key that identifies the node                                      |

---

## Node Identity

A node's identity is its Ed25519 signing key fingerprint. `ryeos-remote` already has a signing key (configured at `SIGNING_KEY_DIR`) and exposes the public key via `/public-key` for TOFU pinning.

The node ID is derived from the key — not assigned, not arbitrary. `fp:4b987fd4e40303ac` is the fingerprint of the node's Ed25519 public key. Same principle as principal identity in the [Decentralized Rye OS](encrypted-shared-intelligence.md) doc: the key IS the identity.

---

## Capabilities: Dynamic, Node-Reported

### No static capability declarations

`remote.yaml` stays minimal — just reachability:

```yaml
# .ai/config/cas/remote.yaml
remotes:
  node-1:
    url: https://node-1.internal
    key_env: NODE_API_KEY
  node-2:
    url: https://node-2.internal
    key_env: NODE_API_KEY
  node-3:
    url: https://node-3.internal
    key_env: NODE_API_KEY
```

No capability declarations in config. The node is the source of truth for what it can do.

### `/status` endpoint

Each node exposes `/status` — a lightweight HTTP endpoint that wraps a local `node/status` tool execution. The tool runs through the local `PrimitiveExecutor` (no CAS sync, no remote dispatch), introspects the workspace, and returns:

```json
{
  "node_id": "fp:4b987fd4e40303ac",
  "healthy": true,
  "active": 3,
  "max_concurrent": 8,
  "capabilities": [
    "rye.execute.tool.llm.complete.meta-llama.llama-3-1-70b",
    "rye.execute.tool.llm.complete.meta-llama.llama-3-1-8b",
    "rye.execute.tool.rye.core.runtimes.python.*",
    "rye.execute.tool.rye.core.runtimes.bash.*"
  ]
}
```

**Capabilities are derived from the workspace.** The `node/status` tool scans `.ai/tools/`, builds the capability list from what's actually installed. When you deploy a new tool to the node, it appears in capabilities on the next `/status` query. When you remove one, it disappears. No config changes needed on any other node.

**Capability strings use the existing fnmatch pattern system.** The same matching that governs thread permissions — `rye.execute.tool.<tool_id>` — expresses what tools a node can execute. The routing tool filters nodes by fnmatch against their reported capabilities. No new matching logic.

**Active count is server state.** The `ryeos-remote` server increments a counter when an `/execute` request starts, decrements when it finishes. `/status` reads the counter. No separate status update mechanism.

**The `node/status` tool runs locally.** It's a regular RYE tool, executed through the local `PrimitiveExecutor`. No CAS sync, no remote dispatch overhead. The `/status` HTTP endpoint is a thin wrapper that calls `ExecuteTool.handle("tool", "node/status", ...)` locally and returns the result.

---

## Routing

### Same tool ID, different implementations per node

The key architectural insight: a tool with the same ID can have different implementations on different nodes. This is standard RYE space resolution — project space tools override system space tools.

**On a GPU execution node** serving llama-3.1-8b:

`llm/complete/meta-llama/llama-3-1-8b` calls `model.generate()` directly via tinygrad in-process — formatting via the `llm/format` processor, parsing via the `llm/tool-calls` parser, both reading from `.ai/config/llm/models/meta-llama.yaml`. No HTTP, no server.

**On a CPU-only execution node:**

`llm/complete/meta-llama/llama-3-1-8b` is a different implementation — one that queries `/status` on known remotes, matches capabilities via fnmatch, picks the least loaded node, and dispatches via `_dispatch_remote()`.

Same tool ID. Different behavior. The directive just calls `llm/complete/meta-llama/llama-3-1-8b` and the tool on that node does the right thing. No fallback logic in the executor. No "tool not found locally, try remote" behavior. The routing is intentional — authored into the tool.

### The routing flow

```
ExecuteTool.handle("tool", "llm/complete/meta-llama/llama-3-1-8b", { messages, tools })
  (called internally by the completions server, or by a routing tool on a CPU-only execution node)

  → tool implementation is the routing version
    → reads remote.yaml for known node URLs
    → queries /status on each (cached, TTL-based refresh)
    → fnmatch: "rye.execute.tool.llm.complete.meta-llama.llama-3-1-8b"
      → nodes 3-15 match
    → filter: healthy=true
    → rank: node-9 has lowest active count
    → _dispatch_remote(target="remote:node-9")

  → on node-9: tool implementation is the local version
    → format → tinygrad model.generate() → parse
    → returns structured completion
```

This is the same for any distributed tool execution, not just inference. A browser tool, a data processing tool, a scraper — same routing pattern. The system doesn't know what the tool does. It matches capabilities and dispatches.

---

## The Inference Cluster Case

15 machines, 30 GPUs. Concrete example.

### Setup

**Nodes 1-2** (4x A100 each): GPU execution nodes serving llama-3.1-70b via tinygrad in-process with model parallelism. Their `.ai/tools/` includes `llm/complete/meta-llama/llama-3-1-70b` — calls `model.generate()` directly.

**Nodes 3-15** (2x GPU each): GPU execution nodes serving llama-3.1-8b via tinygrad in-process. Their `.ai/tools/` includes `llm/complete/meta-llama/llama-3-1-8b` — same pattern, different model.

**All 15 nodes** are execution nodes running `ryeos-remote`. All expose `/status`. All report their capabilities dynamically.

**CPU-only execution node** (separate, could be Modal): has routing implementations of the `llm/complete` tools that query `/status`, match capabilities, and dispatch to whichever GPU execution node can serve the requested model.

A separate **completions server** exposes `/v1/chat/completions` — a standalone HTTP service (not an endpoint on `ryeos-remote`) that agent threads and external callers use as a standard LLM provider. It runs RYE's execution engine underneath, routing internally to GPU execution nodes via the same `_dispatch_remote()` path.

### `remote.yaml` on the execution node

```yaml
remotes:
  node-1:
    url: https://node-1.internal
    key_env: NODE_API_KEY
  node-2:
    url: https://node-2.internal
    key_env: NODE_API_KEY
  # ... through node-15
```

Just URLs. Capabilities come from the nodes themselves.

### A request comes through

```
Webhook → execution node → directive "snap-track/add-show" → agent starts

  → agent needs LLM reasoning
    → http_provider → POST /v1/chat/completions
      → completions server routes internally:
        → routing implementation queries /status on nodes 3-15
          → node-9 healthy, active=2, lowest load
          → _dispatch_remote(target="remote:node-9")
            → node-9 executes locally: format → tinygrad generate() → parse
            → returns structured completion with tool_calls

  → agent parses tool_calls
    → ExecuteTool.handle("tool", "browser/goto", { url: "snapchat.com/@mrbeast" })
      → runs locally on execution node (no routing needed, tool exists locally)
      → returns scraped data

  → agent needs another LLM call with tool results
    → http_provider → POST /v1/chat/completions
      → completions server routes: node-12 this time (load shifted)
      → returns final completion

  → agent stores results → done
```

The agent thread uses the provider path for LLM calls (fast, no CAS overhead). Tool dispatch goes through `execute` (local or routed).

### Hard reasoning case

Agent hits an anomaly, needs the 70b model. The completions server is configured with a separate model endpoint (or the agent thread's provider config specifies a different model):

```
  → http_provider → POST /v1/chat/completions { model: "meta-llama/llama-3-1-70b" }
    → completions server routes internally:
      → routing implementation queries /status on all remotes
        → fnmatch "rye.execute.tool.llm.complete.meta-llama.llama-3-1-70b"
          → only nodes 1-2 match
        → node-1 active=1, node-2 active=3
        → _dispatch_remote(target="remote:node-1")
          → tinygrad generate() on 4x A100 (model parallelism)
          → returns
```

Same routing inside the completions server. The model selection is which model the agent requests. The completions server finds a node that can serve it.

### Adding a new model to the cluster

1. Install tinygrad + download model weights on node-3
2. Add the tool file to node-3's `.ai/tools/llm/complete/meta-llama/llama-3-1-70b`
3. Start (or restart) `ryeos-remote` — tinygrad loads the model into GPU memory
4. Node-3's `/status` now reports that capability
5. Execution node picks it up on next cache refresh

No config changes on the execution node. The new node's capabilities are discovered from its `/status` response.

---

## Execution Flow

The existing execution protocol is unchanged:

| Step       | Current                                  | Multi-node                                   |
| ---------- | ---------------------------------------- | -------------------------------------------- |
| 1. Route   | By name (`remote:gpu`)                   | By capability match via `/status`            |
| 2. Sync    | `POST /objects/has` + `/objects/put`     | Same                                         |
| 3. Request | `POST /execute` with Bearer token        | Same (future: per-request Ed25519 signature) |
| 4. Execute | `create_execution_space()`, run executor | Same                                         |
| 5. Results | Return snapshot hash + new object hashes | Same                                         |
| 6. Merge   | `three_way_merge()` fold-back            | Same                                         |

The only new step is routing — querying `/status` and matching capabilities. Everything after route selection uses the existing remote dispatch path.

---

## What `ryeos-remote` Becomes

The same server, deployed on more machines. Nothing about the server code changes for the multi-node case:

|               | Current                   | Multi-node                                  |
| ------------- | ------------------------- | ------------------------------------------- |
| Where it runs | Modal (single deployment) | Any machine — self-hosted or Modal          |
| How many      | One                       | One per node                                |
| CAS storage   | Modal Volume (`/cas`)     | Local NVMe per node                         |
| Identity      | Supabase user_id          | Ed25519 key fingerprint (`fp:...`)          |
| New endpoint  | —                         | `/status` wrapping local `node/status` tool |

The server at `services/ryeos-remote/ryeos_remote/server.py` doesn't assume Modal. Modal-specific parts are isolated in `modal_app.py`. Running the same `server.py` via uvicorn on bare metal works today.

---

## What's New vs What Exists

### Proposed New Items

| Item                         | Type                            | Purpose                                                                                    |
| ---------------------------- | ------------------------------- | ------------------------------------------------------------------------------------------ |
| `node/status`                | Tool                            | Local workspace introspection — scans available tools, reports capabilities + active count |
| `/status` endpoint           | HTTP endpoint on `ryeos-remote` | Thin wrapper around local `node/status` tool execution                                     |
| Routing tool implementations | Tools                           | Per-tool routing implementations that query `/status` and dispatch via `_dispatch_remote`  |

### Proposed New Config

| Config                  | Location              | Purpose                                                                     |
| ----------------------- | --------------------- | --------------------------------------------------------------------------- |
| `cluster/topology.yaml` | `.ai/config/cluster/` | Optional — routing policy (prefer local, tier definitions, load thresholds) |

### What's Unchanged

| Component                              | Status                                                      |
| -------------------------------------- | ----------------------------------------------------------- |
| `ExecuteTool._dispatch_remote()`       | Unchanged — routing tools use existing remote dispatch      |
| `rye/core/remote/remote` tool          | Unchanged — CAS sync + execute, same protocol               |
| `remote_config.py` + `cas/remote.yaml` | Unchanged — just reachability, capabilities come from nodes |
| `ryeos-remote` server                  | Unchanged — same endpoints, deployable on any hardware      |
| CAS sync protocol (`has/put/get`)      | Unchanged                                                   |
| `create_execution_space()`             | Unchanged                                                   |
| `three_way_merge()` fold-back          | Unchanged                                                   |
| `PrimitiveExecutor` chain resolution   | Unchanged — same tool → runtime → primitive chains          |
| Ed25519 signing + integrity            | Unchanged — node identity derived from existing signing key |
| TOFU key pinning                       | Unchanged — already pins remote Ed25519 key                 |
| Thread capabilities + SafetyHarness    | Unchanged — enforced identically on every node              |
| fnmatch pattern matching               | Unchanged — same matching used for node capabilities        |

---

## Relationship to Other Future Docs

| Doc                                                                                           | Relationship                                                                                                                                                                                                                                                 |
| --------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| [Sovereign Inference](sovereign-inference.md)                                                 | Describes the completions server, provider path for agent threads, and how inference calls become tools routed through `execute` on GPU execution nodes. This doc describes how those tools get distributed across nodes.                                    |
| [Decentralized Rye OS](encrypted-shared-intelligence.md)                                      | This doc covers compute routing and node identity. That doc covers the full cryptographic identity model (principals, sealed objects, group keys). The auth migration (Bearer → per-request signatures) connects them, but the compute layer is independent. |
| [Residual Stream & Native Model Family](Residual%20stream%20and%20native%20model%20family.md) | Model family components deploy as tools on nodes. Nodes report which model tools they have via `/status`.                                                                                                                                                    |

---

## Open Design Questions

### Status caching and TTL

The execution node caches `/status` responses to avoid querying every node on every routing decision. The TTL determines how fresh the capability and load data is. Short TTL (seconds) = more accurate routing, more network overhead. Long TTL (minutes) = stale data, less overhead. The right balance depends on how dynamic the cluster is.

### Node health and failover

If a node is unreachable, the routing tool skips it. `/health` already exists on `ryeos-remote`. The routing tool could ping `/health` as a quick check before `/status`, or just treat a failed `/status` query as "unhealthy."

### CAS locality

Nodes that already have a project's CAS objects cached require less sync overhead before execution. The routing tool could prefer nodes with cached objects — but this requires nodes to report which project snapshots they have cached. This could be part of the `/status` response or a separate query.

### Scheduling complexity

The routing tool starts simple — filter by capability, pick least loaded. Over time it could grow: affinity (prefer the node that last ran this project), locality (prefer nodes with cached CAS), tiered routing (different policies per tool type). Each of these is a policy expression in `cluster/topology.yaml`, not a code change to the routing tool.
