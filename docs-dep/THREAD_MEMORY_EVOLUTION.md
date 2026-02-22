# Thread Memory — Full Design Evolution

> Compiled from 6 Amp threads spanning Feb 22 2026. Documents every design decision, reversal, simplification, and open question across the full iteration history.

---

## Thread 1: Initial Design (T-019c829c)
**"Plan track-blox project using ryeos and gemini"**

### Starting Point
The idea originated from planning a Track Blox project build using wave-based orchestration. The problem: when `implement_database` completes and `implement_api` starts, the API thread has no access to the schema decisions, column names, or constraints from the database thread.

### Initial Architecture
- **Embedding**: OpenAI `text-embedding-3-small` via existing `http_client` (no local model)
- **Storage**: SQLite `thread_memory.db` with embeddings as BLOBs
- **Search**: Brute-force cosine similarity (sufficient for <1000 threads)
- **Indexing**: `after_complete` hook triggers `thread_indexer.py`
- **Recall**: Initially proposed as manual injection into system prompt in `runner.py`

### Problems Discovered
1. **No system prompt exists** — `runner.py` has no system prompt. Context is injected via `run_hooks_context()` into the first user message.
2. **Manual injection breaks patterns** — hardcoding recall in `runner.py` would break the data-driven hook pattern used for identity/rules.
3. **Every thread has a knowledge entry** — `render_knowledge()` runs in `runner.py`'s `finally` block, so every completed thread already has a knowledge entry at `.ai/knowledge/agent/threads/{thread_id}.md`.

### Resolution
Shifted to fully hook-driven approach — indexing via `after_complete` hook, recall via `thread_started` hook. Zero modifications to `runner.py` or core files needed. Knowledge entries from `render_knowledge()` are the indexing source.

---

## Thread 2: Future Extensions Alignment (T-019c82c8)
**"Thread memory RAG and future extensions alignment"**

### Key Additions

**Structural path refactor**: All thread data moved from `.ai/threads/` to `.ai/agent/threads/`. Memory storage at `.ai/agent/memory/thread_memory.db`.

**Missing `after_complete` dispatch discovered**: `after_complete` hooks were defined in `hook_conditions.yaml` but never actually dispatched — no code calls `harness.run_hooks("after_complete", ...)`. Fix: add dispatch in `thread_directive.py` after step 14.

**Embedding size constraints**: Full knowledge entries (transcripts) can exceed the 8191-token limit of `text-embedding-3-small`. V1 truncates; V2 would prefer indexing compressed summaries.

**Dual-source indexing introduced**: Two entry types in the same SQLite store:
1. Transcript entries — full turn-by-turn log from `render_knowledge()`
2. Summary entries — compressed output from `thread_summary` directive during handoffs

**Tighter handoff integration**: Summary indexing married to the handoff flow — when `handoff_thread` runs `thread_summary`, the summary thread's `after_complete` triggers the indexer.

---

## Thread 3: Rapport and Event Naming (T-019c82f8)
**"Rapport accumulation format in thread summaries"**

### Event Lifecycle Debate
Significant debate about naming thread lifecycle events. Final decision:
- `thread_started` — fresh thread, new chain
- `thread_continued` — new thread spawned from handoff (inherits chain)
- `thread_resumed` — same thread picking up new message (not an event, internal state)

### Two-Channel Retrieval Formalized
1. **Project Context** (deterministic) — latest-K entries from same project, regardless of similarity. Ensures wave dependencies always met.
2. **Associative Recall** (similarity-based) — cosine similarity across all projects/chains. Surfaces serendipitous relevance.

### XML Injection Format
```xml
<thread_memory project="project-name">
  <project_context>
    <entry directive="..." timestamp="..." type="summary">...</entry>
  </project_context>
  <associative_context>
    <entry directive="..." timestamp="..." type="summary" similarity="0.82">...</entry>
  </associative_context>
</thread_memory>
```

### Deduplication Rules
1. Summary preference — if both transcript and summary exist for same thread, keep only summary
2. Chain exclusion — for `thread_continued`, exclude entries from same `chain_root_id`
3. Cross-channel dedup — if entry qualifies for both channels, keep only in Project channel

### Rapport Decision
"How It Went" section for user preferences/rapport was debated. Concluded that rapport is "state" not "memory" — should be deterministic injection, not RAG. Deferred to v1.1+.

---

## Thread 4: Summary Validation and Simplification (T-019c836b)
**"Thread memory implementation summary validation"**

### Shift to Graph-Based Orchestration
Moved from monolithic `thread_indexer.py` to a state graph (`memory_index_graph.yaml`):
- Chains atomic tools: `memory_config` → `memory_read_knowledge` → `memory_embed` → `memory_store`
- Decoupled from thread runner, triggered by `after_complete` hook

### Project Knowledge Extraction Debate
Problem: durable facts (schemas, API contracts) get lost as they age out of the latest-K window.
- Initially: regex extraction from markdown headings
- Then: XML tag extraction from LLM output
- Final: explicit LLM tool call (`store_project_knowledge`) — a small LLM thread runs as a node in the indexing graph

### Critical Simplification: No Transcript Embedding
**Decision**: Transcripts are no longer embedded. Only summaries and project knowledge are stored.
**Rationale**: Transcripts are too noisy (tool calls, error handling, retries, intermediate reasoning). They produce mediocre embeddings and false-positive recalls.

### Self-Sufficient Summaries
Summaries must be dense enough to act as the primary recall unit. The actual transcript is a secondary reference reachable via `thread_chain_search`.

---

## Thread 5: Major Simplification (T-019c8392)
**"Simplify thread memory RAG model"**

### Removal of Auto-Summarization
**Removed**: The `completion_summary` directive that spawned an LLM call for every completing thread.
**Reason**: Not every thread needs summarization. Extra LLM call per thread is expensive and often low-value. The handoff/continuation mechanism is separate infrastructure.

### Removal of Project Context Channel
**Removed**: The deterministic SQL "latest-K from same project" retrieval channel.
**Reason**: If past work is relevant, embedding similarity surfaces it. If it's not relevant, don't force-inject it. Two retrieval systems fighting each other.

### Removal of Metadata-Only Injection Tier
**Removed**: Below-threshold matches getting metadata-only references (titles/paths/scores).
**Reason**: The model already has `rye search` for on-demand discovery. Metadata-only references are redundant.

### Removal of Reentrant Indexing Graph
**Removed**: Complex pattern where normal completion → spawn completion_summary → its after_complete re-enters the graph.
**Replaced with**: Linear 4-step graph: config → read → embed → store. No routing, no re-entry.

### Directive-Controlled Indexing
**Changed**: Memory indexing moved from infra-level (layer 3) to directive-declared (layer 1). Each directive opts in via its hooks section. Directives that produce meaningful work declare it; trivial ones don't.

### Embedding Strategy Change
**Changed**: Stored docs embed `"{directive_name}\n{directive_body}\n---\n{knowledge_content}"` (instructions + output). Query embeds `"${directive_body}"` (the full instructions). Both sides in same semantic space — instructions match instructions.

### What Survived
- Single-channel RAG (similarity only)
- Single injection tier (full load or nothing)
- Data-driven scoring: `threshold = max(min_floor, best_score * score_ratio)`
- Hook-based lifecycle (after_complete for index, thread_started/continued for recall)
- SQLite vector index with per-row model versioning
- No new dependencies (stdlib only + http_client)

---

## Thread 6: Current Thread (T-019c83ee)
**"Oracle review + architectural rethinking"**

### Oracle Review Findings (Must-Fix)
1. `_build_prompt()` output too noisy for embedding — use clean directive body (name + description + process steps), not the built prompt with XML tags/permissions/boilerplate
2. `messages.insert(0, ...)` wrong for continuation threads — should inject near last user message
3. `after_complete` in fork path is outside async context — must dispatch from `runner.py`'s `finally` block, not `thread_directive.py`

### Oracle Review Findings (Should-Fix)
4. `chain_root_id` not available at recall time — use `exclude_thread_id` (previous thread's ID) instead
5. Project scoping missing from recall query — add `WHERE project = ?`
6. 2000 token budget + no per-item truncation = inject 1 big item or nothing — add per-item truncation

### `render_knowledge()` Clarification
The doc had been calling these "knowledge entries" as if they were concise. In reality, `render_knowledge()` renders the **full thread transcript** — all cognition events, tool calls, results — as a signed markdown file. These can be massive.

### Fundamental Rethinking: Known Dependencies vs Unknown Relevance

**Key insight**: Embedding and querying entire directives + transcripts to rediscover what the orchestrator already knows is backwards.

The orchestrator wrote the wave plan. It knows Wave 2 needs Wave 1's output. So wire it up directly:

```yaml
# implement_api directive hooks:
hooks:
  - event: "thread_started"
    action:
      primary: "execute"
      item_type: "knowledge"
      item_id: "agent/threads/implement_database/implement_database-..."
```

This is deterministic, zero API calls, zero false positives. RAG is for **unknown relevance** — things that are useful but nobody thought to wire up.

### Two-Layer Architecture
1. **Declared knowledge hooks** — known dependencies wired by the orchestrator (deterministic, ~80% of cross-thread context)
2. **RAG** — serendipitous discovery of undeclared relevance (decisions, discoveries, constraints from past threads that nobody thought to wire up)

### Generic RAG Primitives
RAG shouldn't be thread-memory-specific. It should be a generic `rye/rag/` toolset:
- `rag_embed.py`, `rag_store.py`, `rag_query.py`, `rag_reindex.py`
- Namespaced SQLite index (consumers don't collide)
- Any consumer, any corpus (agent memory, personality, user-defined)
- Agent memory is just one consumer with two agent-specific files

### Handoff Summary Decoupling
The current `handoff_thread()` hardcodes a `thread_summary` directive spawn on every handoff. This should be hook-driven like everything else — directives that want summarization declare it, infrastructure doesn't force it.

### Open Question: What to Index for RAG
Full transcripts are noisy. The high-signal content is:
- Decisions ("chose `game_id UUID` over `INTEGER` because...")
- Discoveries ("Roblox API rate limits at ~100 req/min")
- Constraints ("RLS policies require `auth.uid()` in every query")
- Workarounds ("had to use `pg_notify` because Supabase webhooks don't...")

V1 indexes a truncated transcript excerpt (coarse but automatic). V2 adds a `memory_write` tool for explicit LLM-authored entries (dense and precise, but requires prompting).

### Open Question: Indexing Trigger
Should indexing be:
- Directive-declared (layer 1) — each directive opts in
- Infra-level (layer 3) — all threads indexed automatically, gated by config

Not resolved.

---

## Summary of What's Been Removed Across All Threads

| Feature | Added In | Removed In | Reason |
|---|---|---|---|
| Manual injection in `runner.py` | Thread 1 | Thread 1 | Breaks hook pattern |
| Transcript embedding | Thread 2 | Thread 4 | Too noisy, false positives |
| Dual-source indexing (transcript + summary) | Thread 2 | Thread 4 | Simplified to summary-only |
| `store_project_knowledge` LLM extraction | Thread 4 | Thread 5 | Over-engineered |
| Project context SQL channel (latest-K) | Thread 3 | Thread 5 | Fights RAG — if relevant, similarity surfaces it |
| Auto-summarization (`completion_summary` directive) | Thread 4 | Thread 5 | Not every thread needs it, expensive |
| Reentrant indexing graph | Thread 4 | Thread 5 | Complex, risk of loops |
| Metadata-only injection tier | Thread 5 | Thread 5 | Redundant with `rye search` |
| XML injection format | Thread 3 | Thread 6 | Standard `<knowledge>` tags instead |
| `chain_root_id` for exclusion | Thread 3 | Thread 6 | Not available at recall time |
| Thread-memory-specific tools (`rye/memory/`) | Thread 1 | Thread 6 | Generalized to `rye/rag/` |
| Hardcoded handoff summary in orchestrator | (existing) | Thread 6 | Should be hook-driven |
| Single-channel RAG as primary context path | Thread 5 | Thread 6 | Known deps handled by declared hooks; RAG for undeclared only |

## Summary of What Survived

| Feature | Status |
|---|---|
| SQLite vector index, WAL mode, busy_timeout | Survived all threads |
| No new dependencies (stdlib + http_client) | Survived all threads |
| Hook-driven lifecycle | Survived all threads, expanded |
| `after_complete` dispatch prerequisite | Identified Thread 2, refined through Thread 6 |
| Event split (thread_started vs thread_continued) | Identified Thread 3, survived |
| Data-driven scoring (`max(min_floor, best * ratio)`) | Introduced Thread 5, survived |
| Per-row embedding model versioning | Introduced Thread 2, survived |
| Graceful no-op when disabled | Survived all threads |

## Open Questions (Unresolved)

1. **What exactly gets indexed for RAG?** V1 indexes truncated transcript excerpt (automatic, coarse). V2 adds `memory_write` tool (explicit, precise). Neither feels fully right.
2. **When does indexing trigger?** Directive-declared (opt-in) vs infra-level (opt-out)? Unresolved.
3. **Is RAG even the right mechanism?** The user's frustration at the end of Thread 6 suggests the whole approach may still be wrong. The declared knowledge hooks handle known dependencies cleanly. What exactly does RAG add that's worth the complexity?
4. **Handoff summary as hook** — the decoupling is agreed on but not designed. What event does the summary hook fire on? How does the summary text flow into `resume_messages`?
5. **What's the right content for embedding?** Evolved from metadata-only → full transcript → truncated transcript → "decisions and discoveries". The ideal content is small, dense, high-signal — but how to extract it without an LLM call (which was removed in Thread 5)?
