```yaml
id: knowledge-runtime-arc-agi-3-design
title: "Knowledge Runtime Design — ARC-AGI-3 as Reference Implementation"
description: Design reasoning document for the knowledge runtime, using arc-agi-3 as the concrete reference workload. Covers execution model, trust loop, composition semantics, and open design decisions.
category: future
tags: [rust, runtimes, knowledge, arc, trust, signing, composition, context]
version: "0.1.0"
status: active
```

# Knowledge Runtime Design — ARC-AGI-3 as Reference Implementation

> **Purpose:** This is a reasoning document. Hand it to an LLM and ask it to reason through the design decisions. It contains everything needed: the spec, the real workload, the execution model, and the open questions.

---

## 1. What We're Building

A Rust binary (`knowledge-runtime`) that serves as Rye OS's **context composition engine**. It's one of three runtimes (alongside directive-runtime and graph-runtime), each spawned by the daemon as subprocesses via Lillux.

The knowledge runtime does not run LLM loops or walk DAGs. It resolves, traverses, composes, and delivers context from the knowledge graph to consumers. It is the **integrity boundary** — nothing enters an LLM's context without passing through signature verification.

### The Three Runtimes

| Runtime | Pattern | Core Loop |
|---------|---------|-----------|
| directive-runtime | Agent loop | Prompt → LLM call → Tool dispatch → Repeat |
| graph-runtime | DAG walk | Select node → Dispatch → Bind output → Repeat |
| **knowledge-runtime** | **Graph traversal + composition** | **Resolve entry → Traverse edges → Compose block → Budget-fit → Return** |

---

## 2. The Execution Model (Why Trust Matters)

This is the part that constrains all design decisions.

### How Rye Execution Works

1. A **directive** declares context: which knowledge items to inject into its LLM prompt
2. The **directive-runtime** bootstraps and needs to materialize that context
3. It calls the **knowledge-runtime** to compose context for each position (system, before, after)
4. The knowledge runtime traverses the knowledge graph, verifying every node's signature
5. Only verified content enters the LLM prompt
6. The LLM reasons and produces outputs
7. If the agent creates new knowledge (through a Rye tool), it gets **signed on creation by the agent's identity key**
8. That new knowledge becomes part of the graph — future compositions will traverse and verify it

### The Trust Loop

```
sign → compose → verify → inject into prompt → reason → act → learn → sign → ...
```

It's closed. Every piece of knowledge the LLM ever sees has been verified as authored by a trusted identity. Nothing enters the prompt unverified.

- **Signing key = identity.** Items signed by the current agent's key are trusted. Items signed by other keys depend on the trust store. Unsigned items are rejected.
- **Knowledge created at runtime is signed.** The `tool:arc/knowledge write_pattern` function is a Rye tool. When it writes a new knowledge file, Rye's tool execution pipeline signs it. There are no "temporary" or "unsigned" knowledge items. Everything goes through the signing pipeline.
- **The knowledge runtime is the integrity boundary.** It verifies signatures on every node during traversal. If verification fails, that node and its descendants are excluded from composition.

### What This Means for the Runtime

- Signature verification is not optional or a "nice-to-have" — it's the core purpose
- The `validate` operation isn't for catching typos — it's for ensuring no tampered content gets injected into the LLM prompt
- The `query` operation searches within the **trusted knowledge set** only — unsigned items never appear in results
- The `compose` operation's `items_omitted_reason` field tracks `integrity_failure` alongside `over_budget`

---

## 3. The Reference Workload: arc-agi-3

arc-agi-3 is a knowledge-driven game-playing agent for the ARC-AGI-3 Interactive Reasoning Benchmark. It is the reference workload for the knowledge runtime — the real thing that exercises every operation.

### What arc-agi-3 Does

The agent plays grid-based games using an observe → think → act loop. The key insight: **reasoning is free** — only submitted actions count toward the score. So the agent spends unlimited compute planning between actions.

The think pipeline runs between every game action:

1. **Observe** — Parse current grid state
2. **Recall** — Query knowledge graph for matching patterns and strategies
3. **Plan** — Rank candidate actions using pattern confidence, loop detection, heuristics
4. **Learn** — Buffer outcomes, periodically write discovered patterns back to knowledge

Training runs offline at ~2000 FPS with zero API cost using parallel exploration and Monte Carlo Tree Search. Knowledge accumulates in a graph of markdown files.

### The Knowledge Graph (18 nodes)

The graph has a **star topology** with `arc/foundation` as the center hub:

```
arc/foundation (root hub — referenced by 13 of 17 other nodes)
├── references → arc/action-interface        (leaf root)
├── references → arc/scoring-methodology     (leaf root)
├── references → arc/game-structure          (leaf root)
├── references → arc/strategy/reasoning-first (confidence: 0.95)
│   ├── extends: foundation
│   ├── references → arc/strategy/evolutionary-loop (0.90)
│   │   ├── extends: foundation, reasoning-first
│   │   └── references → training-guide, rich-history
│   ├── references → arc/strategy/rich-history (0.85)
│   │   ├── extends: foundation, reasoning-first
│   │   └── references → action-interface
│   ├── (all 6 strategy nodes extend foundation + reasoning-first)
│   └── arc/strategy/cross-game-transfer (deepest chain: 3 levels)
│       ├── extends: foundation, reasoning-first, evolutionary-loop
│       └── references → patterns/index, action-abstractions
├── arc/training-guide
│   ├── extends: foundation
│   └── references → action-interface, scoring, patterns/index
├── arc/patterns/index
│   ├── extends: foundation
│   └── references → movement, spatial, state-transitions
└── arc/infrastructure/knowledge-runtime-requirements
    ├── extends: foundation
    └── references → strategy cluster
```

Terminal leaves: `action-interface`, `scoring-methodology`, `game-structure`, `patterns/movement`, `patterns/spatial`, `patterns/state-transitions`, `game_patterns/avoid_re86_100` (anti-pattern, confidence 0.70).

### How Knowledge Is Currently Consumed (Broken)

The `tool:arc/think` `_recall()` function does raw filesystem reads:

```python
def _recall(game_id, think_depth, project_path):
    from arc.knowledge import KnowledgeStore
    ks = KnowledgeStore(project_path)
    game_knowledge = ks.read(f"arc/games/{game_id}")    # single file
    all_patterns = ks.read_all("arc/game_patterns")      # directory glob
```

It doesn't traverse `extends`. Doesn't follow `references`. Doesn't verify signatures. Doesn't budget tokens. If a directive declares `context: [arc/foundation]`, the current executor returns *only* the body of `foundation.md` — not the action-interface, scoring, game-structure, or reasoning-first strategy that `foundation` references.

The directive `train.md` already describes the desired state:

```yaml
# Current (flat reads — must manually list every item)
context:
  system: [arc/foundation]
  before: [arc/training-guide, arc/patterns/index]

# Target (graph composition — runtime traverses from entry)
context:
  system:
    - arc/games/{{game_id}}       # traversal pulls in the full graph
  before:
    - arc/strategy/reasoning-first  # pulls strategy context chain
```

### How Knowledge Is Created (Already Signed)

New knowledge is written during gameplay through Rye tools:

- `tool:arc/think learn` — buffers outcomes, writes patterns (anti-patterns, win sequences)
- `tool:arc/explore` — writes game mechanics and patterns discovered during exploration
- `tool:arc/mcts` — writes optimal strategies per game

All go through Rye tool execution → signed by the agent's identity at write time. The existing `tool:arc/knowledge` handles the write pipeline. Newly created items get proper frontmatter with `extends`, `references`, `tags`, `confidence`, and are signed.

---

## 4. The Five Operations

### 4.1 `resolve` — Single Item Lookup

Read one knowledge item, verify signature, strip frontmatter, return body.

```
Input:  { "operation": "resolve", "item_id": "arc/foundation" }
Output: { "status": "success", "content": "<body>", "metadata": {...}, "resolved_from": "project" }
```

This replaces the current Python executor's behavior exactly. Parity target.

### 4.2 `compose` — Graph-Aware Context Composition (CRITICAL)

The primary operation. Traverse the knowledge graph from an entry item, compose a context block within a token budget.

```
Input:  { "operation": "compose", "item_id": "arc/games/ls20",
          "depth": 2, "token_budget": 4000, "exclude_items": [] }
Output: { "status": "success", "content": "<composed markdown>",
          "composition": { "resolved_items": [...], "items_omitted": [...], "edges": [...] },
          "tokens_used": 3847, "token_budget": 4000 }
```

#### Composition Algorithm

```
compose(entry_ref, depth, budget):

  1. RESOLVE ENTRY — three-tier search, verify signature, parse frontmatter

  2. TRAVERSE (BFS with dedup)
     while queue not empty:
       a. pop (ref, remaining_depth, role)
       b. if ref already resolved: record edge, skip
       c. resolve ref → verify signature → parse frontmatter
       d. add to resolved set
       e. if remaining_depth > 0:
          - push each extends ref with (depth-1, "extends")
          - push each references ref with (depth-1, "reference")

  3. ORDER
     a. extends chain first (bottom-up: foundational before dependent)
     b. primary entry
     c. references (by tag overlap with entry, then depth, then name)

  4. RENDER + BUDGET
     for each item in sorted order:
       rendered = render_item(item)  // ## header + body + separator
       item_tokens = chars / 4
       if tokens_used + item_tokens > budget:
         record omission ("over_budget"), continue
       append to output

  5. RETURN composed content + metadata
```

BFS with a `resolved` set inherently prevents cycles. Depth=0 is resolve-only. Default depth: 2. Hard cap: 8.

#### Signature Verification During Traversal

At step 2c, every resolved node must pass signature verification:

- **Pass:** node is included in composition
- **Fail:** node is excluded, `items_omitted_reason` records `"integrity_failure"`, `knowledge_integrity_failure` hook fires
- **Descendants of a failed node are not traversed** — if a node fails verification, its extends/references are not pushed to the queue. A compromised node cannot pull unverified content into the composition through its graph edges.

#### Deduplication Across Positions

When directive-runtime composes context for multiple positions (system, before, after), it tracks what's been loaded and passes `exclude_items` to subsequent calls:

```json
{
  "operation": "compose",
  "token_budget": 2000,
  "exclude_items": ["arc/foundation", "arc/action-interface"]
}
```

The runtime skips these and fills the budget with new context.

### 4.3 `query` — Search Across Knowledge Base (HIGH PRIORITY)

Structured search across knowledge items. BM25 over markdown content — no embedding models needed.

```
Input:  { "operation": "query", "query": "maze wall following trapped",
          "scope": "arc", "filters": { "min_confidence": 0.5 }, "limit": 5 }
Output: { "status": "success", "results": [...ranked with snippets] }
```

Search space is already filtered by trust — unsigned items never appear in results.

### 4.4 `graph` — Knowledge Graph Inspection (MEDIUM)

Traverse and return the graph structure without composing content.

```
Input:  { "operation": "graph", "item_id": "arc/foundation", "direction": "outbound", "max_depth": 3 }
Output: { "nodes": [...], "edges": [...], "depth_reached": 2, "cycle_detected": false }
```

Use case: before playing a game, check knowledge coverage. Dense graph → trust pre-computed strategy. Sparse graph → fall back to exploration-heavy play.

### 4.5 `validate` — Subgraph Integrity Check (NICE TO HAVE)

Verify integrity and completeness of a knowledge subgraph. Pre-submission check for competition.

```
Input:  { "operation": "validate", "item_id": "arc/foundation", "check_references": true, "max_depth": 3 }
Output: { "valid": true, "items_checked": 7, "warnings": [...], "unsigned": [], "signature_failures": [] }
```

---

## 5. Integration with Directive Runtime

### Current (Broken)

```
for position in (system, before, after):
    for kid in context[position]:
        result = daemon_callback("execute", item_id=f"knowledge:{kid}")
        bodies.append(result.content)  # flat file read, no traversal
    context[position] = "\n\n".join(bodies)
```

N tool calls for N context items. No graph awareness. No budget.

### Target (With Knowledge Runtime)

```
for position in (system, before, after):
    kids = context[position]
    result = daemon_callback("execute", item_id=f"knowledge:{kids[0]}",
                             inputs={ "operation": "compose",
                                      "depth": config.knowledge_default_depth,
                                      "token_budget": position_budget[position],
                                      "exclude_items": already_loaded })
    context[position] = result.content
```

One composition call per position. The runtime handles graph traversal, deduplication, signature verification, and budget fitting. Directive-runtime gets richer, verified context with less configuration.

---

## 6. Binary Contract

```
knowledge-runtime --project-path /path [--thread-id T-xxx]

stdin:  LaunchEnvelope JSON
stdout: result JSON
exit 0: all handled outcomes (including errors in result JSON)
exit non-zero: bootstrap/crash only
```

The daemon doesn't branch on `"knowledge"`. It reads `runtime_binary` from the kind schema and spawns that binary. Same code path for all runtimes.

### What the Runtime Does on the Filesystem

- Reads knowledge files directly (not through daemon callbacks)
- Verifies signatures using the same trust logic as the engine
- Does NOT call LLM providers
- Callbacks to daemon limited to: `runtime.append_event`, `runtime.finalize_thread`

### Callbacks and Thread Lifecycle

Knowledge operations are short-lived (milliseconds to seconds). No continuation, no interruption, no command delivery.

```rust
ThreadKindProfile {
    root_executable: true,
    supports_interrupt: false,
    supports_continuation: false,
}
```

---

## 7. Implementation Phases

### Phase 0 — Keep Python Executor
Ship directive-runtime and graph-runtime first. Knowledge stays on Python. No changes.

### Phase 1 — knowledge-runtime with `resolve`
Exact parity with current Python executor. Validates the daemon launch pipeline for knowledge items.

### Phase 2 — `compose` operation
Graph traversal, composition, budget fitting. Wire into directive-runtime's context materialization. **This is the critical deliverable for arc-agi-3.**

### Phase 3 — `query` and `graph` operations
BM25 search, graph inspection. Wire query into MCP fetch tool's search mode.

### Phase 4 — `validate` operation
Subgraph validation. Wire into knowledge signing workflow.

---

## 8. Design Decisions to Reason About

These are the open questions. They require thinking through tradeoffs, not just implementation.

### 8.1 Composition Policy: Extends-First vs Entry-First

The spec says: extends chain first, then primary entry, then references.

The arc-agi-3 project argues: for game-playing, the game-specific knowledge (entry item) is more immediately useful than foundational definitions. They want entry-first ordering.

**Question:** Should the default ordering be configurable per-project? The spec already has `knowledge.yaml` with `budget_strategy: prioritize_primary`. Should there be an `ordering_policy` field too?

```yaml
composition:
  ordering: extends_first   # default — foundational context first
  # ordering: entry_first   # arc-agi-3 preference — game-specific first
```

Or should the ordering be a parameter on the compose call itself, letting the directive decide?

**Considerations:**
- For the LLM consuming the composed context, foundational knowledge first gives the model grounding before encountering specifics
- For a deterministic agent (competition mode, no LLM), game-specific knowledge first gives the planner the most actionable information immediately
- The ordering affects reasoning quality — an LLM that sees specific examples before general principles reasons differently than one that sees principles first
- The `position` parameter (system vs before vs after) already provides some ordering control

### 8.2 Tag-Based Inclusion vs Pure Graph Traversal

The spec's compose algorithm only traverses `extends` and `references` edges. The arc-agi-3 project wants **tag-based inclusion** — items whose tags overlap with the entry item should also be considered for composition, even if they're not directly connected by graph edges.

**Example:** `arc/games/ls20` has tags `[arc, game, ls20, maze]`. `arc/patterns/navigate_maze` has tags `[arc, pattern, movement, maze]`. They share `arc` and `maze` tags but have no graph edge. Tag-based inclusion would pull `navigate_maze` into the composition.

**Question:** Is tag-based inclusion a separate operation, or an extension of compose?

**Approaches:**
- A. Add `tag_matching: true` to compose — extends the BFS to also explore tag-overlapping items
- B. Keep compose as pure graph traversal, use `query` separately for tag matching — let the caller compose the results
- C. Add a `compose + query` hybrid operation that does graph traversal first, then supplements with tag-matched items if budget remains

**Considerations:**
- Tag matching is fundamentally different from graph traversal — it's a search operation, not a resolution operation
- Items found by tag matching have no graph relationship to verify — they're pulled in by keyword overlap, not by authored relationships
- But in arc-agi-3, the tag overlap IS the relationship — the agent wrote those tags specifically to enable cross-game knowledge transfer
- Trust model: tag-matched items still go through signature verification, so they're not less trusted, just differently discovered

### 8.3 Confidence-Weighted Budget Fitting

Some knowledge items have confidence scores (0.0-1.0). Strategy nodes range from 0.75-0.95. Anti-patterns at 0.70.

**Question:** Should the composition algorithm use confidence as a priority signal for budget fitting?

Currently the spec truncates by priority group (extends → primary → references). Within each group, items are ordered by visit_order. No consideration of confidence.

**Approaches:**
- A. Confidence doesn't affect ordering — it's metadata for the consumer (LLM/agent), not for the composer
- B. Within each priority group, sort by confidence (descending) before budget fitting
- C. Low-confidence items (below threshold, e.g. 0.5) are excluded from composition entirely unless explicitly requested
- D. Confidence affects the budget — high-confidence items get more of the token budget (weighted allocation)

**Considerations:**
- Confidence is game-specific context — the knowledge runtime shouldn't have domain-specific logic
- But the spec already has `traversal.extends_weight` and `traversal.references_weight` as priority multipliers
- Confidence could be a generic priority signal: items with higher confidence are more likely to be correct, so prefer them in budget-constrained situations
- The runtime could expose confidence in the composition metadata without using it for ordering — let the consumer decide

### 8.4 The `compress` Budget Strategy

The spec mentions `compress` as a future budget strategy: "summarize low-priority items." Currently only `prioritize_primary` and `truncate_tail` are defined.

arc-agi-3's `knowledge-runtime-requirements.md` says they don't need it: "Just truncate."

**Question:** Is truncation sufficient, or will compression become necessary as knowledge bases grow?

**Considerations:**
- arc-agi-3 currently has 18 nodes. A well-trained agent might have 100-200 nodes (one per game + patterns + primitives).
- With depth=2 and 200 nodes, a compose call could touch 30-50 items. At 200-500 tokens each, that's 6000-25000 tokens — well beyond typical budgets.
- Truncation means losing items entirely. Compression means keeping a summary. A summary of 10 pattern files is more useful than 2 full pattern files.
- But compression requires an LLM (or a very good summarization heuristic). The knowledge runtime doesn't call LLMs.
- Alternative: the runtime could do **structured truncation** — for each omitted item, include a one-line header with title + tags, so the consumer knows what was available but excluded.

### 8.5 Incremental Composition Within a Session

During a single game session, the agent's knowledge grows:
- The learn phase writes new patterns after each action
- These patterns get signed and become part of the graph
- Subsequent compose calls could include them

**Question:** Should the knowledge runtime cache traversal state within a session, or re-traverse from scratch each time?

**Approaches:**
- A. Stateless — every compose call is a fresh traversal. Simple, correct, no cache invalidation needed.
- B. Session-scoped cache — the runtime (or a wrapper) caches the last composition result and does incremental updates when new items are added.
- C. The runtime is stateless, but the directive-runtime maintains composition state and passes `exclude_items` + `include_items` to subsequent calls.

**Considerations:**
- The runtime is a subprocess — it spawns fresh for each call. Caching requires shared state (filesystem, daemon-managed).
- Session-scoped knowledge growth is small (a few patterns per game). The overhead of re-traversal is minimal.
- But the `exclude_items` mechanism already handles this at the directive-runtime level. The knowledge runtime doesn't need to know about sessions.
- The right answer is probably A — keep the runtime stateless, push session awareness to the caller.

### 8.6 Breadth of Trust Verification

When composing context for a directive, the knowledge runtime verifies signatures. But against which trust store?

**Question:** Does the knowledge runtime use the calling directive's identity, its own identity, or a project-level trust store?

**Considerations:**
- If the directive's identity: different directives may have different trust levels. A training directive might trust more items than a competition directive.
- If the project trust store: all items signed by any trusted key in the project are included. This is the current model.
- If the runtime's own identity: the runtime is a system binary — it doesn't have an "identity" in the user/agent sense.
- The answer is probably the project trust store — the runtime verifies against the same trust configuration that the daemon uses for all item verification. This is consistent with the spec's §Dependencies: "Runtime does NOT bypass trust checks" and uses "the same trust logic as the engine."

---

## 9. What Success Looks Like

### For arc-agi-3

Before the knowledge runtime, a directive for game play must manually enumerate every knowledge item in its context declaration. The author must know the full graph. When new strategies are added, every directive must be updated.

After the knowledge runtime, the directive declares:
```yaml
context:
  system: [arc/games/{{game_id}}]
```

One compose call with depth=2 pulls in the game strategy, foundation, action interface, scoring, reasoning-first, evolutionary-loop, rich-history, and all their references — verified, deduplicated, within budget. The agent's context is automatically enriched as training adds more knowledge to the graph.

### For Rye OS

Knowledge transforms from **passive files that agents read** into an **active context graph that the system navigates**. Authors define relationships in frontmatter; the runtime traverses them; consumers get the right context without knowing the full graph structure. And the trust loop is closed: everything in the LLM's context has been verified as authored by a trusted identity.

---

## 10. Files Referenced

### Spec
- `docs/future/knowledge-runtime.md` — full knowledge runtime specification
- daemon launch pipeline, callback system, runtime_binary dispatch (see `docs/future/mcp-end-to-end-bug-sweep.md`)

### arc-agi-3 Knowledge Graph
- `.ai/knowledge/arc/foundation.md` — root hub (extends nothing, referenced by 13 nodes)
- `.ai/knowledge/arc/action-interface.md` — 7-action interface leaf
- `.ai/knowledge/arc/scoring-methodology.md` — RHAE scoring leaf
- `.ai/knowledge/arc/game-structure.md` — grid/state machine leaf
- `.ai/knowledge/arc/training-guide.md` — training methodology (extends foundation)
- `.ai/knowledge/arc/patterns/index.md` — pattern taxonomy (extends foundation, refs pattern leaves)
- `.ai/knowledge/arc/strategy/reasoning-first.md` — core strategy (confidence 0.95)
- `.ai/knowledge/arc/strategy/cross-game-transfer.md` — deepest dependency chain (confidence 0.75)
- `.ai/knowledge/arc/infrastructure/knowledge-runtime-requirements.md` — arc-agi-3's own requirements doc for the runtime

### arc-agi-3 Tools
- `.ai/tools/arc/think.py` — the observe/recall/plan/learn pipeline (current broken recall)
- `.ai/tools/arc/knowledge.py` — knowledge read/write store
- `.ai/tools/arc/explore.py` — parallel game exploration
- `.ai/tools/arc/mcts.py` — Monte Carlo Tree Search

### arc-agi-3 Directives
- `.ai/directives/arc/train.md` — training pipeline (shows current vs target context)
- `.ai/directives/arc/think/plan.md` — action selection (consumes recalled knowledge)
- `.ai/directives/arc/think/recall.md` — knowledge retrieval (shows desired query interface)
