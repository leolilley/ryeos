```yaml
id: knowledge-runtime
title: "Knowledge Runtime: Context Composition Engine"
description: A native Rust runtime for knowledge items — graph-aware context composition, token-budget-aware truncation, structured query, and integrity validation across the knowledge graph.
category: future
tags: [rust, runtimes, knowledge, context, composition, graph-traversal, query, search]
version: "0.1.0"
status: planned
```

# Knowledge Runtime: Context Composition Engine

> **Status:** Planned — knowledge is the only item kind without a native runtime. Currently handled by a trivial Python executor (`knowledge.py`, ~70 lines) that strips frontmatter and returns body text. This spec defines what a proper runtime would look like.

> **Prerequisite:** [Native Runtimes](native-runtimes.md) — the daemon launch pipeline, callback capability system, and `runtime_binary` dispatch must be in place.

---

## The Problem

Knowledge items are Markdown documents with YAML frontmatter that define a **knowledge graph** through `extends`, `references`, and `used_by` relationships. But the current executor treats every knowledge item as an isolated file:

1. Find the file (three-tier resolution)
2. Verify integrity (signature check)
3. Parse frontmatter
4. Strip frontmatter
5. Return body text

The graph structure defined in frontmatter is **never traversed at runtime**. Each knowledge item is resolved in isolation. If `Identity.md` declares `extends: [Environment]`, the runtime does not load `Environment.md`. The directive author must manually list every knowledge item they need in their context declaration.

Meanwhile, directive-runtime and graph-runtime are full native binaries with LLM loops and DAG walkers. Knowledge — the system's context backbone — gets a Python script that reads a file.

---

## What the Knowledge Runtime Is

A standalone Rust binary (`knowledge-runtime`), spawned by the daemon via `lillux exec`, that serves as the system's **context composition engine**.

It does not run LLM loops (directive-runtime) or walk DAGs (graph-runtime). It resolves, traverses, composes, and delivers context from the knowledge graph to consumers.

### Computational Pattern

| Runtime | Pattern | Core Loop |
|---------|---------|-----------|
| directive-runtime | Agent loop | Prompt → LLM call → Tool dispatch → Repeat |
| graph-runtime | DAG walk | Select node → Dispatch → Bind output → Repeat |
| **knowledge-runtime** | **Graph traversal + composition** | **Resolve entry → Traverse edges → Compose block → Budget-fit → Return** |

---

## Architecture

### Same Pattern as Other Runtimes

```
ryeosd
  │ resolve knowledge:rye/agent/core/Identity
  │ verify signature + trust
  │ extract kind schema → runtime_binary: "knowledge-runtime"
  │ compute effective limits (shallow — no LLM token spend)
  │ mint callback capability
  │ write thread.json
  │ spawn knowledge-runtime via lillux exec
  │
  ▼ stdin: LaunchEnvelope
knowledge-runtime
  │ read config from .ai/config/rye-runtime/
  │ resolve entry knowledge item from target.path
  │ verify integrity
  │ traverse knowledge graph (extends, references) per config
  │ compose context block within token budget
  │ emit events (resolved, traversed, omitted)
  │ write result to stdout
  │
  ▼ stdout: result JSON
ryeosd
  │ finalize thread
  │ return result to caller
```

The daemon does not branch on `"knowledge"`. It reads `runtime_binary` from the kind schema and spawns that binary. Same code path for all kinds.

### Kind Schema

```yaml
# .ai/config/engine/kinds/knowledge/knowledge.kind-schema.yaml
location:
  directory: knowledge
execution:
  runtime_binary: "knowledge-runtime"
formats:
  - extensions: [".md"]
    parser_id: markdown/frontmatter
    signature:
      prefix: "<!--"
      suffix: "-->"
  - extensions: [".yaml", ".yml"]
    parser_id: yaml/yaml
    signature:
      prefix: "#"
metadata:
  rules:
    name: { from: path, key: name }
    version: { from: path, key: version }
    category: { from: path, key: category }
```

No `default_executor_id`. No native adapter registration. One binary, all operations.

---

## Operations

The runtime determines the operation from the LaunchEnvelope's `request.inputs`:

### 1. `resolve` — Single Item Lookup

The floor case. Read one knowledge item, strip frontmatter, return body.

```
Input:
  target.item_id: "knowledge:rye/agent/core/Identity"
  request.inputs: { "operation": "resolve" }

Output:
  {
    "status": "success",
    "content": "<body text>",
    "item_id": "rye/agent/core/Identity",
    "metadata": {
      "name": "identity-core",
      "version": "1.0.0",
      "category": "rye/agent/core",
      "tags": ["identity", "agent", "core"],
      "extends": ["environment"],
      "references": ["behavior", "tool-protocol"]
    },
    "resolved_from": "system",
    "tokens_used": 847
  }
```

This replaces the current Python executor's behavior exactly.

### 2. `compose` — Graph-Aware Context Composition

The primary operation. Traverse the knowledge graph, compose a context block within a token budget.

```
Input:
  target.item_id: "knowledge:rye/agent/core/Identity"
  request.inputs: {
    "operation": "compose",
    "depth": 3,
    "token_budget": 4000,
    "position": "system"
  }

Output:
  {
    "status": "success",
    "content": "<composed markdown with headers>",
    "composition": {
      "resolved_items": [
        { "item_id": "rye/agent/core/Environment", "role": "extends", "depth": 1 },
        { "item_id": "rye/agent/core/Identity", "role": "primary", "depth": 0 },
        { "item_id": "rye/agent/core/Behavior", "role": "reference", "depth": 1 },
        { "item_id": "rye/agent/core/ToolProtocol", "role": "reference", "depth": 1 }
      ],
      "edges": [
        { "from": "Identity", "to": "Environment", "type": "extends" },
        { "from": "Identity", "to": "Behavior", "type": "references" },
        { "from": "Identity", "to": "ToolProtocol", "type": "references" }
      ],
      "items_omitted": [],
      "items_omitted_reason": {}
    },
    "tokens_used": 3847,
    "token_budget": 4000,
    "tokens_remaining": 153
  }
```

### 3. `query` — Search Across Knowledge Base

Structured search across knowledge items in the three-tier space.

```
Input:
  request.inputs: {
    "operation": "query",
    "query": "how does signing work",
    "scope": "rye/core",
    "limit": 5
  }

Output:
  {
    "status": "success",
    "results": [
      {
        "item_id": "rye/core/signing-and-integrity",
        "title": "Signing and Integrity",
        "relevance": 0.92,
        "snippet": "Every item in Rye OS is cryptographically signed...",
        "entry_type": "reference"
      },
      {
        "item_id": "rye/primary/sign-semantics",
        "title": "Sign Semantics",
        "relevance": 0.87,
        "snippet": "The sign operation writes an Ed25519 signature...",
        "entry_type": "reference"
      }
    ],
    "total_matching": 12,
    "returned": 2
  }
```

### 4. `graph` — Knowledge Graph Inspection

Traverse and return the graph structure without composing content.

```
Input:
  target.item_id: "knowledge:rye/agent/core/Identity"
  request.inputs: {
    "operation": "graph",
    "direction": "outbound",
    "max_depth": 3
  }

Output:
  {
    "status": "success",
    "root": "rye/agent/core/Identity",
    "nodes": [
      { "item_id": "rye/agent/core/Identity", "entry_type": "reference" },
      { "item_id": "rye/agent/core/Environment", "entry_type": "reference" },
      { "item_id": "rye/agent/core/Behavior", "entry_type": "reference" },
      { "item_id": "rye/agent/core/ToolProtocol", "entry_type": "reference" }
    ],
    "edges": [
      { "from": "Identity", "to": "Environment", "type": "extends" },
      { "from": "Identity", "to": "Behavior", "type": "references" },
      { "from": "Identity", "to": "ToolProtocol", "type": "references" }
    ],
    "depth_reached": 2,
    "cycle_detected": false
  }
```

### 5. `validate` — Subgraph Integrity Check

Verify integrity and completeness of a knowledge subgraph.

```
Input:
  target.item_id: "knowledge:rye/agent/core/Identity"
  request.inputs: {
    "operation": "validate",
    "check_references": true,
    "max_depth": 3
  }

Output:
  {
    "status": "success",
    "valid": true,
    "items_checked": 7,
    "warnings": [
      "Behavior references 'streaming-patterns' which does not exist in any tier"
    ],
    "stale": [
      { "item_id": "rye/agent/core/Environment", "last_validated": "45 days ago" }
    ],
    "unsigned": [],
    "signature_failures": []
  }
```

---

## Composition Algorithm

The `compose` operation is the core of the runtime.

```
compose(entry_ref, depth, budget, position):

  1. RESOLVE ENTRY
     - Resolve entry item via three-tier search
     - Verify signature against trust store
     - Parse frontmatter → extract metadata + body

  2. INITIALIZE TRAVERSAL STATE
     - queue = [(entry_ref, depth, "primary")]
     - resolved = {}                    // item_id → { body, metadata, role, depth }
     - edges = []                       // (from, to, type)
     - visit_order = []                 // insertion order for rendering

  3. TRAVERSE (BFS with dedup)
     while queue not empty:
       a. pop (ref, remaining_depth, role) from queue
       b. if ref already in resolved: record edge, skip resolve
       c. resolve ref → verify signature → parse frontmatter
       d. add to resolved[item_id] = { body, metadata, role, remaining_depth }
       e. append to visit_order
       f. if remaining_depth > 0:
          - for each ref in metadata.extends:
              push (ref, remaining_depth - 1, "extends")
              record edge (current → ref, "extends")
          - for each ref in metadata.references:
              push (ref, remaining_depth - 1, "reference")
              record edge (current → ref, "references")

  4. ORDER
     Sort resolved items by composition policy:
       a. extends chain first (bottom-up: foundational before dependent)
       b. primary entry
       c. references (ordered by: tag overlap with entry, then depth, then name)
     Within each group, respect visit_order for stability.

  5. RENDER + BUDGET
     tokens_used = 0
     for each item in sorted order:
       rendered = render_item(item)   // header + body + separator
       item_tokens = count_tokens(rendered)
       if tokens_used + item_tokens > budget:
         record omission with reason "over_budget"
         continue
       append rendered to output
       tokens_used += item_tokens

  6. RETURN
     composed_content = join(output parts)
     return { content, composition metadata, token accounting }
```

### Cycle Detection

BFS with a `resolved` set inherently prevents cycles. If item A references B and B references A, the second encounter of A is skipped (dedup) and the edge is recorded, but A is not re-resolved.

### Depth Semantics

- `depth=0` — resolve only the entry item, no traversal. Equivalent to `resolve` operation.
- `depth=1` — entry + direct extends/references.
- `depth=N` — traverse N hops outward from entry.
- Default from config: 2.
- Hard cap from config: 8.

### Token Counting

Approximation: `chars / 4`. No need to pull in a tokenizer — knowledge runtime doesn't need exact counts, just budget enforcement. The directive-runtime that consumes the composed context will do its own exact accounting against the model's context window.

---

## Config

The runtime loads knowledge-specific config from `.ai/config/rye-runtime/knowledge.yaml`, resolved three-tier (system → user → project). Schema validated at load time like all runtime config.

### `knowledge.yaml`

```yaml
# Knowledge runtime configuration
composition:
  default_depth: 2
  max_depth: 8
  cycle_detection: true
  budget_strategy: prioritize_primary
  # prioritize_primary — extends chain and primary are kept; references truncated first
  # truncate_tail       — remove items from the end until within budget
  # compress            — future: summarize low-priority items

traversal:
  extends_weight: 1.0       # priority multiplier for extends chain
  references_weight: 0.7    # priority multiplier for lateral references
  deduplicate: true         # skip items already resolved via another path

rendering:
  include_headers: true     # wrap each item in a ## header with its title
  header_level: 2           # markdown heading level for item headers
  separator: "\n\n---\n\n"  # separator between items
  include_item_id: true     # include item_id in headers for traceability

search:
  field_weights:
    title: 3.0
    name: 3.0
    tags: 2.5
    description: 2.0
    category: 1.5
    content: 1.0
  max_results: 10
```

### Config Schema

`knowledge.config-schema.yaml` validates the above structure. Loaded and verified at bootstrap alongside all other runtime config.

---

## Module Layout

```
knowledge-runtime/
├── Cargo.toml
└── src/
    ├── main.rs        — CLI + stdin/stdout contract (LaunchEnvelope in, result JSON out)
    ├── bootstrap.rs   — Config loading, kind schema resolution, three-tier setup
    ├── resolve.rs     — Three-tier knowledge item resolution + signature verification
    ├── frontmatter.rs — YAML frontmatter parsing for .md and .yaml knowledge items
    ├── compose.rs     — Graph traversal, ordering, budget-fitting, composition
    ├── query.rs       — Search across knowledge base (BM25 + tag/category matching)
    ├── graph.rs       — Knowledge graph traversal and edge extraction
    ├── validate.rs    — Subgraph integrity and completeness validation
    ├── render.rs      — Markdown rendering of composed context blocks
    └── budget.rs      — Token counting, budget-aware truncation and omission tracking
```

### Dependencies

```toml
[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
lillux = { path = "../lillux/lillux" }
rye_runtime = { path = "../rye-runtime" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-util", "net"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

No HTTP client — the runtime doesn't call LLM providers. It calls back to the daemon for `runtime.dispatch_action` when it needs to resolve nested items through the daemon's engine (instead of doing raw filesystem reads, which would bypass trust checks).

Wait — actually the runtime has filesystem access and does its own resolution and verification (same pattern as directive-runtime per the native-runtimes-spec §C2: "All filesystem reads use signature verification. Runtime does NOT bypass trust checks."). So it reads files directly and verifies signatures using the same trust logic as the engine. It does NOT call back to the daemon for item resolution.

Callback usage is limited to:
- `runtime.append_event` — emit composition events
- `runtime.finalize_thread` — finalize on completion
- `runtime.dispatch_action` with primary=execute — only if the runtime needs to invoke a tool (e.g., for future structured extraction via a tool)

---

## Binary Contract

```
knowledge-runtime --project-path /path [--thread-id T-xxx]

stdin:  LaunchEnvelope JSON
stdout: result JSON
exit 0: all handled outcomes (including errors in result JSON)
exit non-zero: bootstrap/crash only
```

### main.rs Flow

1. Read LaunchEnvelope from stdin
2. Validate envelope_version
3. Verify target digest matches file on disk
4. Run bootstrap (load config from three-tier roots)
5. Determine operation from `request.inputs.operation` (default: "resolve")
6. Execute operation
7. Emit events via daemon callback
8. Write result to stdout
9. Finalize thread

### LaunchEnvelope for Knowledge

Same envelope structure as directive/graph. The daemon doesn't customize it:

```json
{
  "envelope_version": 1,
  "invocation_id": "inv-...",
  "thread_id": "T-...",
  "roots": {
    "project_root": "/path/to/project",
    "user_root": "/home/user",
    "system_roots": ["/opt/rye/bundles/core", "/opt/rye/bundles/standard"]
  },
  "target": {
    "item_id": "knowledge:rye/agent/core/Identity",
    "kind": "knowledge",
    "path": ".ai/knowledge/rye/agent/core/Identity.md",
    "digest": "sha256:abc123..."
  },
  "request": {
    "inputs": {
      "operation": "compose",
      "depth": 3,
      "token_budget": 4000,
      "position": "system"
    }
  },
  "policy": {
    "effective_caps": ["rye.fetch.knowledge.*"],
    "hard_limits": {
      "duration_seconds": 30
    }
  },
  "callback": {
    "socket_path": "/tmp/ryeosd.sock",
    "token": "cbt-...",
    "allowed_primaries": ["execute", "fetch", "sign"]
  }
}
```

---

## Integration with Directive Runtime

In the native-runtimes-spec, directive-runtime bootstrap step C2 includes:

> 5. Materialize context: resolve knowledge items from context positions, read content, verify signatures.

Currently this step executes each knowledge item individually through the Python executor (N tool calls for N context items). With the knowledge runtime in place:

### Before (current)

```
for position in (system, before, after):
    for kid in context[position]:
        result = daemon_callback("execute", item_id=f"knowledge:{kid}")
        bodies.append(result.content)
    context[position] = "\n\n".join(bodies)
```

### After (with knowledge runtime)

```
for position in (system, before, after):
    kids = context[position]
    result = daemon_callback("execute", item_id=f"knowledge:{kids[0]}",
                             inputs={ "operation": "compose",
                                      "depth": config.knowledge_default_depth,
                                      "token_budget": position_budget[position] })
    context[position] = result.content
    # result.composition tells us exactly what was included
```

One composition call per position. The knowledge runtime handles graph traversal, deduplication, and budget fitting. The directive runtime gets richer context with less configuration.

### Deduplication Across Positions

Knowledge referenced in both `system` and `before` positions is currently loaded twice (two separate tool calls). With composition, the directive runtime can track what's been loaded across positions and pass `exclude_items` to subsequent compose calls:

```json
{
  "operation": "compose",
  "depth": 3,
  "token_budget": 2000,
  "position": "before",
  "exclude_items": ["rye/agent/core/Identity", "rye/agent/core/Environment"]
}
```

The knowledge runtime skips already-loaded items and fills the budget with new context.

---

## Hook Events

The knowledge runtime fires hooks at these events:

| Hook event | When |
|-----------|------|
| `knowledge_resolved` | Single item resolved successfully |
| `knowledge_composed` | Graph composition completed |
| `knowledge_traversal_skip` | Item skipped (already resolved, dedup) |
| `knowledge_budget_exceeded` | Items omitted due to token budget |
| `knowledge_integrity_failure` | Signature or trust check failed for an item in the graph |
| `knowledge_reference_broken` | Referenced item not found in any tier |
| `knowledge_query_completed` | Search query returned results |

These are persisted via `runtime.append_event` and visible in the thread's event stream.

---

## Thread Lifecycle

Knowledge operations are short-lived (milliseconds to seconds). The thread lifecycle is:

```
running → completed
running → failed (integrity failure, bootstrap error)
```

No continuation, no interruption, no command delivery. The thread-kind profile:

```rust
ThreadKindProfile {
    root_executable: true,
    supports_interrupt: false,
    supports_continuation: false,
}
```

---

## Implementation Phases

### Phase 0 — Keep Python Executor

Ship directive-runtime and graph-runtime first. Knowledge stays on the Python executor. No changes.

### Phase 1 — knowledge-runtime with resolve

Implement `resolve` operation only. Exact parity with current Python executor behavior. Validates the daemon launch pipeline works for knowledge items via `runtime_binary`. Replaces the Python executor.

**Tests:**
- Three-tier resolution
- Signature verification
- Frontmatter stripping
- YAML and Markdown formats
- Missing items return error
- Integrity failures return error
- stdin LaunchEnvelope contract
- exit 0 on handled failures

### Phase 2 — compose Operation

Implement graph traversal, composition, budget fitting. Wire into directive-runtime's context materialization.

**Tests:**
- Traversal follows extends chains with depth limits
- Traversal follows references laterally
- Cycle detection prevents infinite loops
- Deduplication works across multiple paths to same item
- Budget truncation omits lowest-priority items
- Composition metadata accurately reports resolved/omitted
- Ordering: extends chain before primary before references
- exclude_items parameter skips already-loaded items

### Phase 3 — query and graph Operations

Implement search and graph inspection. Wire query into MCP fetch tool's search mode.

**Tests:**
- BM25 search with field weights
- Tag-based matching
- Category filtering
- Graph traversal returns correct edges
- Graph handles missing references gracefully
- Depth limiting on graph output

### Phase 4 — validate Operation

Implement subgraph validation. Wire into knowledge signing workflow.

**Tests:**
- Detects broken references
- Detects unsigned items in subgraph
- Detects signature mismatches
- Reports staleness based on validated timestamp
- Respects depth limit during validation

---

## Workspace Integration

Add to `Cargo.toml` workspace:

```toml
[workspace]
members = [
    "ryeosd",
    "ryeos-engine/rye_engine",
    "lillux/lillux",
    "rye-runtime",
    "graph-runtime",
    "knowledge-runtime"
]
resolver = "2"
```

Build order dependency: `knowledge-runtime` depends on `rye-runtime` (shared callback, hooks, paths). No dependency on `directive-runtime` or `graph-runtime`.

---

## Why This Matters

Today, a directive author writes:

```yaml
context:
  system: [identity/core, behavior, tool-protocol, environment, directive-instruction]
```

They have to know the full graph. If `Identity` extends `Environment`, the author still has to list both. If `Behavior` gains a new reference, the author has to update every directive that needs it.

With the knowledge runtime, the directive author writes:

```yaml
context:
  system: [identity/core]
```

And the runtime composes the full context graph: Identity + Environment (its extends) + Behavior, ToolProtocol (its references) — all within the token budget, with deduplication, integrity verified, and metadata about what was included and excluded.

Knowledge transforms from **passive files that agents read** into an **active context graph that the system navigates**. The author defines relationships in frontmatter; the runtime traverses them; the consumer gets the right context without knowing the full graph structure.
