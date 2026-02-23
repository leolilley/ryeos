
```yaml
name: persistence-and-state
title: Persistence and State
entry_type: reference
category: rye/agent/threads
version: "1.2.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - persistence
  - state
  - continuation
  - resumption
references:
  - thread-lifecycle
  - limits-and-safety
  - "docs/orchestration/continuation-and-resumption.md"
```

# Persistence and State

How threads persist state, handle context limits via continuation, and support user-driven resumption.

## Storage Layout

```
.ai/agent/threads/
├── registry.db              # Thread registry (SQLite)
├── budget_ledger.db         # Hierarchical budget tracking (SQLite)
├── <thread_id>/             # Thread transcripts
│   ├── thread.json          # Signed thread metadata
│   └── transcript.jsonl     # Append-only event log with checkpoint signatures
└── <graph_run_id>/          # Graph transcripts (same pattern)
    └── transcript.jsonl     # Graph events, checkpoint-signed

.ai/knowledge/agent/threads/
├── <directive>/<thread_id>.md   # Thread knowledge transcript (signed)
└── <graph_id>/<graph_run_id>.md # Graph knowledge transcript (signed)

.ai/knowledge/graphs/
└── <graph_id>/<graph_run_id>.md # Graph state for resume (signed JSON)
```

### Thread Registry (`registry.db`)

In-memory + persistent SQLite. Tracks all threads across the project.

| Column                   | Purpose                                |
| ------------------------ | -------------------------------------- |
| `thread_id`              | Primary key                            |
| `directive`              | Directive name                         |
| `parent_id`              | Parent thread (null for root)          |
| `status`                 | Current state                          |
| `continuation_thread_id` | Forward pointer in continuation chain  |
| `continuation_of`        | Backward pointer in continuation chain |
| `chain_root_id`          | First thread in continuation chain     |
| `result`                 | Final result (JSON serialized)         |
| `cost`                   | Cost snapshot (JSON)                   |
| `created_at`             | Creation timestamp                     |
| `updated_at`             | Last update timestamp                  |

### Budget Ledger (`budget_ledger.db`)

SQLite-backed hierarchical cost tracking. Ensures threads can't overspend.

| Column           | Purpose                                      |
| ---------------- | -------------------------------------------- |
| `thread_id`      | Primary key                                  |
| `parent_id`      | Parent for cascading                         |
| `max_spend`      | Budget ceiling                               |
| `reserved_spend` | Amount reserved (for active children)        |
| `actual_spend`   | Actual spend (includes cascaded child costs) |
| `status`         | active / completed / error                   |

### `thread.json`

Written at thread creation, updated at finalization:

```json
{
  "thread_id": "agency-kiwi/discover_leads-1739820456",
  "directive": "agency-kiwi/discover_leads",
  "status": "completed",
  "created_at": "2026-02-17T10:00:56+00:00",
  "updated_at": "2026-02-17T10:01:23+00:00",
  "model": "claude-3-5-haiku-20241022",
  "limits": { "turns": 10, "tokens": 200000, "spend": 0.1 },
  "capabilities": ["rye.execute.tool.scraping.gmaps.scrape_gmaps"],
  "cost": {
    "turns": 4,
    "input_tokens": 3200,
    "output_tokens": 800,
    "spend": 0.02
  },
  "outputs": { "leads_file": ".ai/data/leads.json", "lead_count": "15" }
}
```

When a directive declares `<outputs>` and the LLM calls `directive_return`, the thread result includes an `outputs` dict with the structured key-value pairs. This is available in the thread result returned to parent threads via `wait_threads`, and in the `thread.json` metadata.

Signed with a `_signature` field using canonical JSON serialization. Protects capabilities and limits from tampering. Verified on resume and handoff.

### `transcript.jsonl`

Append-only JSONL event log. Each line is a JSON object with `timestamp`, `thread_id`, `event_type`, and `payload`. Checkpoint events are interleaved at turn boundaries with SHA256 hash and Ed25519 signature covering all preceding bytes.

### Knowledge Entry (`.ai/knowledge/agent/threads/{directive}/{thread_id}.md`)

Signed knowledge entry with cognition-framed markdown. Contains YAML frontmatter with thread-specific fields (`thread_id`, `directive`, `status`, `model`, `turns`, `spend`) and `entry_type: thread_transcript`. Updated at each checkpoint and finalization. Discoverable via `rye search knowledge`.

### Graph Transcripts

Graph executions use the same two-stream observability pattern as threads:

1. **JSONL event log** — `.ai/agent/threads/{graph_run_id}/transcript.jsonl`. Append-only, checkpoint-signed at step boundaries using the same `TranscriptSigner`.
2. **Knowledge markdown** — `.ai/knowledge/agent/threads/{graph_id}/{graph_run_id}.md`. Contains a visual node status table (✅ completed, 🔄 running, ⏳ pending, ❌ error) and event history. Re-rendered from JSONL at each step (overwritten, not appended). Signed via `MetadataManager.create_signature`.

Graph event types:

| Event | Emitted When |
|-------|-------------|
| `graph_started` | Walker begins execution |
| `step_started` | A node begins executing |
| `step_completed` | A node finishes successfully |
| `foreach_completed` | A foreach iteration completes |
| `graph_completed` | All nodes finished, graph succeeds |
| `graph_error` | A node fails and the graph halts |
| `graph_cancelled` | Graph execution is cancelled |

Key differences from thread transcripts:

- **No SSE streaming** — graphs don't produce tokens; events are discrete
- **No `TranscriptSink`** — events are written directly by the walker (`walker.py`)
- **Overwrite, not append** — knowledge markdown is fully re-rendered from JSONL at each step
- **State file separate** — resumable graph state lives at `.ai/knowledge/graphs/{graph_id}/{graph_run_id}.md` (signed JSON), unchanged from before

Cross-process polling in `orchestrator.py` (`_poll_registry`) uses flat 500ms intervals (not exponential backoff).

## Context Limit Detection

After every turn, the runner estimates context usage:

```python
tokens_used = len(content) // 4        # ~4 chars per token approximation
context_limit = provider.context_window or 200000
usage_ratio = tokens_used / context_limit

threshold = coordination_config.get("trigger_threshold", 0.9)
if usage_ratio >= threshold:
    trigger_handoff()
```

## Automatic Handoff (Continuation)

When context limit reached (default 90%), five-phase handoff:

### Phase 1: Fill Trailing Messages

Fill the resume ceiling budget with recent messages (most recent first):

```python
trailing_messages = []
for msg in reversed(messages):
    msg_tokens = len(str(msg.get("content", ""))) // 4
    if trailing_tokens + msg_tokens > resume_ceiling:
        break
    trailing_messages.insert(0, msg)
```

Trimmed to start with `user` message (provider requirement).

### Phase 2: Build Resume Messages

```python
resume_messages = [
    *trailing_messages,
    {"role": "user", "content": "Continue executing the directive."},
]
```

### Phase 3: Spawn New Thread

New thread with same directive and resume messages:

```python
new_result = await thread_directive.execute({
    "directive_name": directive_name,
    "resume_messages": resume_messages,
    "parent_thread_id": original_parent_id,
    "previous_thread_id": thread_id,
})
```

Inherits same parent relationship → appears as sibling of original.

`previous_thread_id` enables `thread_continued` hooks in the new thread.

### Phase 4: Link Continuation Chain

```python
registry.set_continuation(old_thread_id, new_thread_id)
# old thread: status → "continued", continuation_thread_id → new_thread_id

chain_root_id = registry.get_chain(old_thread_id)[0]["thread_id"]
registry.set_chain_info(new_thread_id, chain_root_id, old_thread_id)
```

### Phase 5: Log Handoff

Recorded in old thread's transcript with new thread ID and trailing turn count.

## The Continuation Chain

Linked list of threads representing a single logical execution:

```
Thread A (continued) → Thread B (continued) → Thread C (completed)
```

Each thread stores:

- `continuation_thread_id` — forward pointer
- `continuation_of` — backward pointer
- `chain_root_id` — first thread in chain

### Chain Resolution

```python
def resolve_thread_chain(thread_id, project_path):
    current = thread_id
    visited = set()  # prevents infinite loops from corrupted data
    while True:
        if current in visited:
            return current
        visited.add(current)
        thread = registry.get_thread(current)
        if not thread or thread.get("status") != "continued":
            return current
        continuation_id = thread.get("continuation_thread_id")
        if not continuation_id:
            return current
        current = continuation_id
```

**Wait resolution:** Waiting on thread A that was continued to B then C → returns C's result automatically.

### View a Chain

```python
rye_execute(item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "get_chain", "thread_id": "my-directive-1739820456"})
```

### Search Across a Chain

```python
rye_execute(item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "chain_search", "thread_id": "...",
                "query": "error.*timeout", "search_type": "regex"})
```

Searches transcript knowledge entries across all threads in the chain.

## User-Driven Resumption

Resume any completed, errored, or cancelled thread:

```python
rye_execute(item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "resume_thread",
        "thread_id": "my-directive-1739820456",
        "message": "API key fixed. Retry the scraping step."
    })
```

### Resume Flow

1. **Resolve chain** — follow to terminal thread
2. **Validate state** — must be terminal (`completed`, `error`, `cancelled`). Running/created → rejected
3. **Reconstruct messages** — parse transcript back into message array
4. **Append user message** — new message added to conversation
5. **Spawn as sibling** — same directive, same parent as original
6. **Link chain** — original gets `continuation_thread_id` pointing to new thread
7. **Log** — `thread_resumed` event in original transcript

### Resume vs Handoff

| Aspect       | Automatic Handoff                                                    | User Resume                          |
| ------------ | -------------------------------------------------------------------- | ------------------------------------ |
| Trigger      | Context limit (90%+)                                                 | User calls `resume_thread`           |
| Summary      | Hook-driven — directives opt in via `after_complete` hooks           | No summary — full transcript rebuilt |
| Context      | Trailing messages (within ceiling) + `thread_continued` hook context | Full reconstruction + new message    |
| Overflow     | Runner will handoff again if needed                                  | Same — runner handles if too big     |
| Chain        | Old → New linked                                                     | Old → New linked                     |
| Relationship | Sibling (same parent)                                                | Sibling (same parent)                |

## Example: Hook-Driven Summary on Handoff

Summarization is opt-in. A directive declares hooks for it:

```xml
<hooks>
  <!-- Summarize when thread completes or is handed off -->
  <hook id="summarize_on_complete" event="after_complete">
    <condition path="cost.turns" op="gte" value="1" />
    <action primary="execute" item_type="tool" item_id="rye/agent/threads/thread_summary">
      <param name="thread_id">${thread_id}</param>
    </action>
  </hook>

  <!-- Re-inject summary + context into continuation thread -->
  <hook id="reinject_summary" event="thread_continued">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.previous_summary_id}" />
  </hook>

  <hook id="reinject_api_schema" event="thread_continued">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.api_thread_id}" />
  </hook>
</hooks>
```

Flow: Thread A hits context limit → `after_complete` runs summary → `handoff_thread()` spawns Thread B with `previous_thread_id` and `inputs` containing the summary ID → `thread_continued` hooks fire in Thread B, re-injecting the summary and dependency context near the last user message.

Without these hooks, handoff still works — continuation thread gets trailing messages only.

**Note:** These are **directive hooks** (XML in `<metadata>`), which handle thread-level knowledge wiring. When using a state graph as the pipeline scaffold, **graph hooks** (YAML in `config.hooks`) handle pipeline-level events (`graph_started`, `after_step`, `graph_completed`). Both use the same underlying infrastructure but operate at different layers. See the orchestrator-patterns knowledge entry for the full relationship.

## Configuration

All continuation settings in `.ai/config/agent/coordination.yaml`:

| Setting                        | Default | Description                                 |
| ------------------------------ | ------- | ------------------------------------------- |
| `trigger_threshold`            | `0.9`   | Context ratio triggering handoff            |
| `resume_ceiling_tokens`        | `16000` | Max tokens for trailing messages in handoff |
| `wait_threads.default_timeout` | `600.0` | Default wait timeout (seconds)              |
