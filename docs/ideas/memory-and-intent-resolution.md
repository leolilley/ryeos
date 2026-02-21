```yaml
id: memory-and-intent-resolution
title: "Memory & Intent Resolution"
description: Shared cross-thread memory with embedding retrieval, intent resolution via RAG + small model, and predictive pre-fetching during streaming
category: ideas
tags: [memory, intent, rag, embedding, thread-memory, pre-fetching, search]
version: "0.1.0"
status: design-proposal
```

# Memory & Intent Resolution

> **Status:** Design Proposal — grounded in existing infrastructure, not scheduled for implementation.

## Executive Summary

As Rye scales in usage, two distinct problems emerge:

1. **Context amnesia** — agents lose access to relevant past work across long thread chains, even though that history exists in completed thread transcripts
2. **Intent precision** — as the `.ai/` registry grows with more directives, tools, and knowledge items, agents increasingly hallucinate tool call syntax or call the wrong item entirely

This document proposes three complementary upgrades that solve both problems while staying true to Rye's core philosophy: everything is data, everything is a tool, override at any layer.

---

## What Exists Today

Before describing what this proposal adds, here's the infrastructure it builds on — all of this is live in the codebase:

| Component                  | Location                                                        | What It Does                                                                                                                                                                                                                                                                                                                     |
| -------------------------- | --------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Thread continuation        | `coordination.yaml` (`trigger_threshold: 0.9`)                  | At 90% context window, hands off to a new thread with a structured summary                                                                                                                                                                                                                                                       |
| `thread_summary` directive | `rye/agent/threads/thread_summary.md`                           | Generates summaries with four sections: Completed Work, Pending Work, Key Decisions & Context, Tool Results                                                                                                                                                                                                                      |
| Transcript persistence     | `.ai/threads/{thread_id}/transcript.jsonl`                      | Full JSONL transcripts stored per thread                                                                                                                                                                                                                                                                                         |
| Thread registry            | `thread_registry.py` (SQLite)                                   | Tracks `thread_id`, `directive`, `model`, `status`, `parent_id`, `continuation_of`, `continuation_thread_id`, `chain_root_id`, `turns`, `input_tokens`, `output_tokens`, `spend`                                                                                                                                                 |
| `thread_chain_search`      | `rye/agent/threads/internal/thread_chain_search.py`             | Regex search across JSONL transcripts within a single delegation tree (resolves ancestry via registry)                                                                                                                                                                                                                           |
| `state_graph_walker`       | `rye/core/runtimes/state_graph_walker.py`                       | Follows `continuation_of` links to retrieve results from continued threads                                                                                                                                                                                                                                                       |
| `rye_search`               | `rye/rye/tools/search.py`                                       | BM25 + fuzzy matching over the registry — field-weighted scoring, boolean operators, wildcards, phrase matching, namespace filtering                                                                                                                                                                                             |
| Event system               | `events.yaml`                                                   | Lifecycle: `thread_started`, `thread_completed`, `thread_suspended`, `thread_cancelled`; Cognition: `cognition_in`, `cognition_out`, `cognition_out_delta`, `cognition_reasoning`; Tools: `tool_call_start`, `tool_call_result`; Errors: `error_classified`, `limit_escalation_requested`; Orchestration: `child_thread_started` |
| Hook system                | `hook_conditions.yaml`                                          | Hook events: `error`, `limit`, `context_window_pressure`, `after_step`, `after_complete`, `directive_return` — each triggers an `execute` action against existing tools/directives                                                                                                                                               |
| Four MCP tools             | `search`, `load`, `execute`, `sign`                             | The entire agent-facing interface                                                                                                                                                                                                                                                                                                |
| Capability attenuation     | fnmatch patterns (e.g., `rye.execute.tool.rye.bash.bash`)       | Granular permission control                                                                                                                                                                                                                                                                                                      |
| Three-tier spaces          | project → user → system with shadow-override                    | Resolution precedence for all items                                                                                                                                                                                                                                                                                              |
| Lilux primitives           | `subprocess`, `http_client`, `signing`, `integrity`, `lockfile` | The full set of OS-level primitives — no embedding primitive exists today                                                                                                                                                                                                                                                        |

---

## Part 1: Shared Thread Memory

### The Problem

Rye already handles context exhaustion gracefully — at 90% of the context window, a thread hands off to a continuation with a summary. `thread_chain_search` already provides regex search across transcripts within a single delegation tree. `state_graph_walker` follows `continuation_of` links to retrieve results from continued threads.

What none of this does is let any thread recall semantically relevant work from _past, unrelated_ threads — whether its own or another agent's. That history exists in `.ai/threads/*/transcript.jsonl` but is dark once a delegation tree is complete. Agents can't see across thread tree boundaries at all.

Over time this means agents repeat work that's already been done, lose nuance established in earlier sessions, and can't draw on the collective experience of the entire agent ecosystem even when it's directly applicable.

### What This Proposes

A **shared cross-thread embedding store** that extends the existing thread infrastructure:

- `thread_chain_search` already provides word search **within** a delegation tree → this proposal extends that to a shared store covering **all** threads
- The thread registry already tracks full provenance metadata → the embedding store indexes against that same metadata
- The hook system already fires on thread lifecycle events → indexing hooks attach to the existing `thread_completed` event (already defined in `events.yaml`)

### One Store, All Threads

Thread memory is a single shared store covering all threads across all agents. There's no architectural distinction between "my threads" and "other agents' threads" — the store is just an embedding index, and whose threads are in it is purely a metadata question handled by the capability system, exactly as permissions work everywhere else in Rye.

Every indexed thread carries provenance metadata drawn from the existing registry schema:

```json
{
  "thread_id": "thread_abc123",
  "directive": "rye/outreach/email_campaign",
  "model": "claude-sonnet-4-20250514",
  "parent_id": "thread_parent_456",
  "chain_root_id": "thread_root_789",
  "status": "completed",
  "project": "ryeos",
  "space": "project",
  "timestamp": "2026-02-19T10:30:00Z",
  "turns": 12,
  "spend": 0.034
}
```

An agent that wants to filter to its own directive queries with a metadata filter on `directive`. An agent that wants everything across a project filters on `project`. An unfiltered query surfaces the full set of threads the agent has permission to see. This is a query detail, not an architectural concern — the store itself makes no distinction.

### Permissions via Capability System

Access to thread memory follows the same capability attenuation model as everything else in Rye. No new permission concepts needed:

```
rye.load.memory.thread.*                      — read any thread in scope
rye.load.memory.thread.project.ryeos.*        — read only ryeos project threads
rye.load.memory.thread.directive.rye.outreach.*  — read only outreach threads
```

Capabilities cascade down the thread hierarchy as normal — a child thread can only access thread memory its parent granted it. A sandboxed scoring leaf that should only see its own work gets exactly that. An orchestrator coordinating across multiple agents can see across all of them.

Space scoping maps onto the existing three-tier system:

- **Project-scoped threads** — visible to all agents operating in that project
- **User-scoped threads** — visible across all your projects
- **System-scoped threads** — shared standard library threads, if any

### Two Complementary Exploration Modes

The agent has two distinct ways to explore thread memory, and both matter:

**Word search / exact recall** — the agent knows what it's looking for. "Find every thread where we called the maps scraper." "Show me all turns across any agent that discussed rate limiting." Fast, precise, transparent. `thread_chain_search` already does this within delegation trees; the proposal extends the same regex search to the full shared store across all thread trees.

**RAG / associative discovery** — the agent doesn't know exactly what's relevant, it just knows what it's working on right now. RAG surfaces past threads — from any agent — that are semantically related to the current conversation. RAG also gives the agent a better starting position for word search — it surfaces where to look, then word search navigates precisely within that.

They compound well in practice. RAG surfaces a relevant past thread → the agent uses word search to drill into the exact turns that matter → gets precise detail rather than just a summary. **RAG is the map; word search is the navigation once you know where you're going.**

### The Thread Summary as the Memory Unit

> **Note:** This section proposes an **extension** to the existing summary format. Today, `thread_summary` produces four sections: Completed Work, Pending Work, Key Decisions & Context, and Tool Results. This proposal adds a fifth section.

The thread summary is the core unit of the memory system — and it could carry more than just a record of the work. Each summary would have the existing four sections plus a new one: **How It Went**, where rapport, working style, user preferences, and relational context live.

**Proposed addition to the summary format:**

```
## How It Went
Leo thinks in systems and arrives at the right answer through intuition —
the useful move is articulating why, not explaining from scratch. Pushed back
early on baking RAG into the registry search; that unlocked the cleaner
architecture. Prefers responses without preamble. Vocabulary is fully shared
at this point — no need to re-establish context on internals.
```

This section would carry forward naturally:

- **Explicit preferences** — "keep responses shorter", "skip bullet points", "don't hedge" — captured when expressed mid-conversation, carried forward in the next summary
- **Implicit patterns** — the model notices the user always pushes back when responses are too long, prefers working through implications conversationally, likes direct disagreement — these emerge from the interaction and belong in the summary just as much as explicit requests
- **Shared vocabulary** — terms, concepts, and shorthand that have been established don't need re-explaining in future threads
- **Relational tone** — how direct, how formal, how much context-setting is needed

This section would be refined at each thread boundary. New patterns override old ones. Corrections made mid-thread make it into the next summary. The user model is always current because it's always written from the most recent interaction.

This is how personality and rapport accumulate without a separate query mechanism. It's in the summary, which is already part of the query vector for everything else. No anchor questions, no separate corpus query, no metadata tagging. The relational context arrives because the summary always arrives.

### Architecture

#### Proposed Thread Memory Tools

These are **new tools** — none of these exist in the codebase today:

```
.ai/
└── tools/
    └── rye/memory/
        ├── thread_rag          # RAG retrieval over shared thread store (NEW)
        ├── thread_search       # Word/regex search over shared thread store (NEW)
        │                       # (extends thread_chain_search from single tree → all threads)
        ├── thread_indexer      # Called on thread completion to index the thread (NEW)
        └── thread_store/       # Shared embedding store (NEW)
            ├── index.db
            └── metadata.json
```

Both retrieval tools are standard Rye data-driven Python tools — signed, versioned, overridable at project or user space.

#### New Lilux Primitive Required

Lilux currently has five primitives: `subprocess`, `http_client`, `signing`, `integrity`, `lockfile`. This proposal requires a sixth:

- **`embedding`** — a new Lilux primitive for computing embeddings, following the same pattern as `http_client` (stateless, configurable, OS-level capability)

#### What Gets Indexed

Each completed thread contributes two things to the store:

**Thread summary** — all sections (including the proposed "How It Went" section). Tagged with full provenance metadata from the thread registry. This is the primary retrieval unit and the carrier of relational context forward through the linked list.

**Transcript chunks** — the full `transcript.jsonl` split into overlapping windows (e.g., 20-turn windows with 5-turn overlap), each chunk tagged with provenance. These preserve detail the summary compresses away, retrievable when a summary match warrants deeper recall.

Indexing happens via a hook on the existing `thread_completed` lifecycle event (already defined in `events.yaml`). The hook would be a new entry in `hook_conditions.yaml`:

```yaml
# Proposed addition to hook_conditions.yaml
- id: "index_thread_memory"
  event: "after_complete" # existing hook event
  layer: 3
  action:
    primary: "execute"
    item_type: "tool"
    item_id: "rye/memory/thread_indexer"
    params:
      thread_id: "${thread_id}"
```

#### Querying: Deterministic from Thread State

There is no query formulation step and no LLM involved in retrieval. The query vector is built from the prior thread summary (if one exists) combined with the last N turns of the live thread. The summary provides continuity across the context boundary — the query reflects where the conversation has been as well as where it is now.

```python
# The thread state IS the query — no formulation step
def query_thread_memory(
    thread_context: list[Turn],
    prior_summary: str | None = None,
    top_k: int = 5,
    max_tokens: int = 500,
    filter: dict = None
) -> list[MemoryResult]:
    query_text = (prior_summary + "\n\n" if prior_summary else "") + turns_to_text(thread_context[-last_n_turns])
    query_embedding = embed(query_text)  # via proposed Lilux embedding primitive
    results = vector_store.search(query_embedding, top_k=top_k, filter=filter)
    return truncate_to_token_budget(results, max_tokens=max_tokens)
```

**Configurable token limit** — results are truncated to a configurable token budget before injection. The right value depends on the thread: a long-running orchestrator can afford more memory injection than a tight leaf thread. Tunable per hook invocation. Repeated use across workloads will surface the optimal trade-off.

Results always include provenance metadata so the agent knows where retrieved context came from — which directive, which project, when.

#### Injection Points

**Thread start** — query the shared thread store with the seed context and inject the top-K relevant summaries into the system prompt. All sections of each retrieved summary arrive — the work context and the relational context together.

**Between turns** — a proposed new hook event (`on_turn_complete`, **does not exist today**) would query thread memory and inject relevant context if similarity crosses a threshold. Gated on confidence so it doesn't add noise.

```yaml
# Proposed NEW hook events (not yet in hook_conditions.yaml)
# These would require new event types in events.yaml as well

- id: "memory_on_thread_start"
  event: "thread_started" # existing lifecycle event
  layer: 3
  action:
    primary: "execute"
    item_type: "tool"
    item_id: "rye/memory/thread_rag"
    params:
      top_k: 5
      max_tokens: 500
      inject_into: system_prompt

# This requires a new hook event type — on_turn_complete does not exist
- id: "memory_between_turns"
  event: "on_turn_complete" # PROPOSED — new event type needed
  layer: 3
  action:
    primary: "execute"
    item_type: "tool"
    item_id: "rye/memory/thread_rag"
    params:
      top_k: 3
      max_tokens: 300
      min_similarity: 0.75
      inject_into: next_turn_context
```

#### Linked List + Shared RAG = Full Memory Stack

The existing thread continuation system forms a linked list — each node is a context window, connected by handoff summaries. `thread_chain_search` navigates within a single delegation tree. Shared RAG indexes across all nodes from all agents. Together:

- **Linear recall** (the linked list) — the agent always knows what happened immediately before via the handoff summary, including the current relational context with the user
- **Tree recall** (`thread_chain_search`, existing) — regex search within a delegation tree for exact matches
- **Associative recall** (shared RAG, proposed) — any agent can reach back to any past thread from any agent it has permission to see, if it's semantically relevant
- **Cross-tree exact recall** (`thread_search`, proposed) — word/regex search extended from single-tree to the full shared store

The system gets more useful over time and across agents as the store grows.

---

## Part 2: Intent Resolution

### The Problem

As `.ai/` registries grow — more directives, more tools, more knowledge items — agents increasingly struggle with exact tool call syntax. They hallucinate parameter names, use wrong item IDs, or pick the wrong item entirely. Rye already has the right surface: four MCP tools as the entire agent-facing interface. The problem is that even with four tools, the agent still needs to know exact `item_id` strings and parameter structures for potentially hundreds of items.

### What Exists Today

`rye_search` (`rye/rye/tools/search.py`) already provides sophisticated discovery over the registry:

- BM25 field-weighted scoring (titles weighted higher than content)
- Fuzzy matching with Levenshtein distance
- Boolean operators (AND, OR, NOT), wildcards, phrase matching
- Data-driven extractors per item type
- Namespace filtering

This is a strong foundation. What it doesn't do is take a **natural-language intent** and resolve it to a **complete, validated tool call** — it finds items, but doesn't construct the invocation.

### The Proposal: Intent Syntax + Registry Metadata RAG + Small Model

Rather than requiring the agent to know exact syntax, it expresses intent in natural language. A resolver directive intercepts this, uses RAG over registry metadata to find the best candidate items (complementing `rye_search`'s BM25), and passes them to a small specialized model to construct the actual tool call.

```
# Instead of:
rye_execute(item_type="directive", action="run", item_id="rye/outreach/email_campaign",
            parameters={"target": "tech companies", "limit": 50})

# The agent writes:
[TOOL: run the email campaign directive for tech companies, limit 50]
```

> **Note:** The `[TOOL: ...]` syntax, `intent_resolver`, and structured output model are all **proposed** — none of this exists in the codebase today.

### Registry Metadata RAG

This proposal adds an **embedding index** as a complement to the existing `rye_search` BM25 index. The RAG index is built purely over item **metadata** — not content. Each item contributes:

```json
{
  "item_id": "rye/outreach/email_campaign",
  "type": "directive",
  "description": "Runs an outbound email campaign for a given target and limit",
  "tags": ["outreach", "email", "campaign"]
}
```

Not the full directive YAML, not the tool source code. Two reasons for this:

**Embedding quality** — full content pollutes the embedding. Implementation detail drags the vector away from what an item _is_ toward how it _works_, which is not what you want to match against an intent. Description and type are the semantic signal that actually matters.

**Context cleanliness** — this is harness-side, detached from the agent context window. Metadata embeddings are small, searches are fast, and the structured output model receives compact candidates it can reason over without being overwhelmed by content.

The index is built at sign time — when an item is signed and added to the registry, its metadata is embedded and stored. Cheap to maintain, always in sync.

### Resolution Pipeline

```
[TOOL: intent text]
        │
        ▼
  Intent Parser (regex)
  extracts intent_text + surrounding context
        │
        ▼
  RAG query over registry metadata
  query vector = embed(intent_text + conversation context)
  returns top-K candidate items (metadata only)
  — deterministic, no LLM in this step —
        │
        ▼
  Small structured-output model (e.g., Gemma 2B/7B)
  input: intent + conversation context + candidate metadata
  output: validated rye_execute(...) call
  — small model, optimized for structured output —
        │
        ▼
  Tool Executor (existing, unchanged)
```

### Intent Syntax

```
[TOOL: <natural language description of what you need>]
```

Examples:

```
[TOOL: search for directives about lead generation]
[TOOL: run the google maps scraper for restaurants in Auckland]
[TOOL: load the API rate limiting knowledge entry]
[TOOL: create a new directive for user onboarding workflows]
```

Direct tool calls still work as a fallback for agents that know exact syntax.

### Why a Small Structured-Output Model

The front-end model is optimized for reasoning, not for reliably generating structured XML against a specific schema. A small model (2B/7B class) fine-tuned specifically for structured output is faster, cheaper, and more reliable for this translation task — and it doesn't consume the main model's context window doing it. The model is just another Rye tool, signed and overridable at any space level.

### Updated AGENTS.md Addition

```markdown
## Tool Calling

Express tool needs naturally using intent brackets:

    [TOOL: description of what you need]

Examples:

- `[TOOL: search for email campaign directives]`
- `[TOOL: run the maps scraper for tech companies in Auckland]`
- `[TOOL: load the bootstrap directive]`
- `[TOOL: create a new knowledge entry about rate limiting]`

The system resolves your intent to the correct tool call. You don't need
to remember exact item IDs or parameter names — just describe what you want.

Direct calls still work if you know the exact syntax.
```

---

## Part 3: Predictive Pre-Fetching

> **Note:** This is entirely new infrastructure — nothing in this section exists in the codebase today.

### The Insight

While the front-end agent is generating its response (1–5 seconds of streaming tokens), we can predict what tool intents it's likely to express and run the registry metadata RAG query _in parallel_. By the time the intent appears in the output, the candidate metadata is already cached. The structured-output model gets its candidates immediately with no wait.

### Architecture

#### Intent Predictor

A small embedding model trained on historical `(conversation_context → intent)` pairs from thread transcripts — the same transcripts that feed thread memory. Because the thread store is shared, the predictor trains on collective agent behavior — a richer signal than any single agent could provide alone.

The predictor would need to fire on a token buffer snapshot during streaming. The existing `cognition_out_delta` event (defined in `events.yaml` as a droppable streaming chunk event) could potentially serve as the trigger, but its current throttle of 0.1 seconds is per-chunk, not per-N-tokens. A new mechanism or event would likely be needed:

```python
# Proposed — fires every ~100 tokens during streaming, non-blocking
predictions = predictor.predict(
    partial_output=buffer[-500:],
    conversation=last_3_turns,
    top_k=3
)
# Fire-and-forget — doesn't block the stream
```

#### Pre-Fetch Cache

Short-lived in-memory cache (TTL ~10 seconds) storing `prediction_embedding → registry metadata candidates`. Cache hit → skip RAG query, straight to structured-output model. Cache miss → normal path.

```
Streaming agent output (~2000ms)
│
├── Every ~100 tokens: predict → RAG query [parallel, non-blocking]
│   └── Cache: {"run email campaign" → [directive_a, directive_b, ...]}
│
└── Agent completes: "[TOOL: run the email campaign for tech companies]"
    └── Cache hit → skip RAG → structured output model → execute
```

#### Hook Integration

This would require a **new hook event type** — `on_token_buffer` does not exist in either `events.yaml` or `hook_conditions.yaml` today:

```yaml
# PROPOSED — new event type and hook condition
# Requires additions to both events.yaml and hook_conditions.yaml

- id: "predictive_prefetch"
  event: "on_token_buffer" # PROPOSED — new event type needed
  layer: 3
  action:
    primary: "execute"
    item_type: "tool"
    item_id: "rye/intent/predictor"
    params:
      buffer_chars: 500
      context_turns: 3
      top_k: 3
      min_confidence: 0.6
      dispatch_prefetch: true
```

---

## Summary: What's New vs What's Unchanged

### Proposed New Tools (all would be signed, all overridable)

| Tool                    | Proposed Location              | Purpose                                                                                     |
| ----------------------- | ------------------------------ | ------------------------------------------------------------------------------------------- |
| `thread_rag`            | `rye/memory/thread_rag`        | RAG over shared thread store                                                                |
| `thread_search`         | `rye/memory/thread_search`     | Cross-tree word/regex search (extends `thread_chain_search` beyond single delegation trees) |
| `thread_indexer`        | `rye/memory/thread_indexer`    | Indexes thread on completion                                                                |
| `intent_resolver`       | `rye/intent/resolver`          | Resolves `[TOOL: ...]` to actual calls                                                      |
| structured output model | `rye/intent/structured_output` | Small LLM for structured output generation                                                  |
| `intent_predictor`      | `rye/intent/predictor`         | Predicts intents during streaming                                                           |

### Proposed New Infrastructure

| Component                         | What It Requires                                                                                       |
| --------------------------------- | ------------------------------------------------------------------------------------------------------ |
| Embedding primitive               | New Lilux primitive (sixth, alongside `subprocess`, `http_client`, `signing`, `integrity`, `lockfile`) |
| Shared thread embedding store     | New persistent store under `.ai/tools/rye/memory/thread_store/`                                        |
| Registry metadata embedding index | New index built at sign time, complements existing BM25 index in `rye_search`                          |

### Proposed New/Extended Hook Events

| Hook Event         | Status                               | Trigger                         | Action                                   |
| ------------------ | ------------------------------------ | ------------------------------- | ---------------------------------------- |
| `after_complete`   | **Existing** — reused for indexing   | Thread exits                    | Index thread to shared store             |
| `thread_started`   | **Existing** — reused for injection  | Thread spawns                   | Inject relevant memory from shared store |
| `on_turn_complete` | **Proposed** — new event type needed | Each agent turn                 | Optional mid-session memory injection    |
| `on_token_buffer`  | **Proposed** — new event type needed | Every N tokens during streaming | Predict + pre-fetch intents              |

### Proposed Extension to Thread Summary

| Section                 | Status                                                  |
| ----------------------- | ------------------------------------------------------- |
| Completed Work          | **Existing**                                            |
| Pending Work            | **Existing**                                            |
| Key Decisions & Context | **Existing**                                            |
| Tool Results            | **Existing**                                            |
| How It Went             | **Proposed** — new fifth section for relational context |

### What's Unchanged

| Component                                            | Location                                            | Status                                                         |
| ---------------------------------------------------- | --------------------------------------------------- | -------------------------------------------------------------- |
| Four MCP tools (`search`, `load`, `execute`, `sign`) | Agent-facing interface                              | Same interface, no changes                                     |
| Capability attenuation (fnmatch patterns)            | Thread memory permissions use the same model        | No new permission concepts                                     |
| Space resolution (project → user → system)           | Three-tier shadow-override                          | Unchanged                                                      |
| Ed25519 signing, lockfiles, chain verification       | Lilux `signing`, `integrity`, `lockfile` primitives | Unchanged                                                      |
| Thread orchestration and budget cascading            | `orchestrator.py`, `coordination.yaml`              | Unchanged                                                      |
| `thread_chain_search`                                | `rye/agent/threads/internal/thread_chain_search.py` | Unchanged — new `thread_search` extends it, doesn't replace it |
| `rye_search` (BM25 + fuzzy)                          | `rye/rye/tools/search.py`                           | Unchanged — RAG index complements it, doesn't replace it       |
| Thread registry (SQLite)                             | `thread_registry.py`                                | Unchanged — embedding store reads from it                      |
| Existing hook events                                 | `hook_conditions.yaml`                              | Unchanged — new hooks are additions, not modifications         |
| Existing lifecycle events                            | `events.yaml`                                       | Unchanged — new events are additions                           |
| Lilux microkernel                                    | `lilux/`                                            | Unchanged except proposed new `embedding` primitive            |

---

## Design Principles Maintained

**Everything is data** — the resolver, predictor, and memory tools are all items in `.ai/`. Swap them by overriding at project level. No framework code changes.

**RAG only where warranted** — registry metadata RAG scoped to intent resolution. Thread memory RAG scoped to associative recall. Neither applied where simpler tools suffice — `rye_search` BM25 remains the primary discovery mechanism, `thread_chain_search` remains the primary within-tree search.

**The runtime runs on itself** — the intent resolution system is subject to the same signing, integrity checks, and space precedence as any other tool.

**Permissions are not a special case** — thread memory access follows capability attenuation exactly as every other resource in Rye. No new permission model needed.

**Fail-closed** — unsigned or tampered tools are rejected. Capability attenuation applies throughout.

**Graceful degradation** — cache miss falls back to normal resolution. Resolution failure falls back to direct tool calls. Memory injection failure is non-fatal. Each layer degrades independently.
