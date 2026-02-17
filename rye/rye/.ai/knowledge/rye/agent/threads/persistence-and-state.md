<!-- rye:signed:2026-02-17T23:54:02Z:a907f94429ba0d3d67c8041c7938e71a82ed5a12e65ad25f94bd5220d20b5e93:uu3vVzoBik5VQlUGxPaQZ-xl4DmHSyUoDKBBD23mZY8CeaJKZ1VRLAa4SCAvmEQnp8xchLWWxDE10ebwLSGlDQ==:440443d0858f0199 -->

```yaml
id: persistence-and-state
title: Persistence and State
entry_type: reference
category: rye/agent/threads
version: "1.0.0"
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
.ai/threads/
├── registry.db           # Thread registry (SQLite)
├── budget_ledger.db      # Hierarchical budget tracking (SQLite)
└── <thread_id>/
    ├── thread.json       # Thread metadata
    └── transcript.md     # Full conversation log
```

### Thread Registry (`registry.db`)

In-memory + persistent SQLite. Tracks all threads across the project.

| Column                  | Purpose                                  |
|-------------------------|------------------------------------------|
| `thread_id`             | Primary key                              |
| `directive`             | Directive name                           |
| `parent_id`             | Parent thread (null for root)            |
| `status`                | Current state                            |
| `continuation_thread_id`| Forward pointer in continuation chain    |
| `continuation_of`       | Backward pointer in continuation chain   |
| `chain_root_id`         | First thread in continuation chain       |
| `result`                | Final result (JSON serialized)           |
| `cost`                  | Cost snapshot (JSON)                     |
| `created_at`            | Creation timestamp                       |
| `updated_at`            | Last update timestamp                    |

### Budget Ledger (`budget_ledger.db`)

SQLite-backed hierarchical cost tracking. Ensures threads can't overspend.

| Column          | Purpose                                      |
|-----------------|----------------------------------------------|
| `thread_id`     | Primary key                                  |
| `parent_id`     | Parent for cascading                         |
| `max_spend`     | Budget ceiling                               |
| `reserved_spend`| Amount reserved (for active children)        |
| `actual_spend`  | Actual spend (includes cascaded child costs) |
| `status`        | active / completed / error                   |

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
  "limits": {"turns": 10, "tokens": 200000, "spend": 0.10},
  "capabilities": ["rye.execute.tool.scraping.gmaps.scrape_gmaps"],
  "cost": {"turns": 4, "input_tokens": 3200, "output_tokens": 800, "spend": 0.02}
}
```

### `transcript.md`

Full conversation log written by `EventEmitter`. Contains user messages, assistant responses, tool calls, tool results, and system events (handoffs, errors).

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

When context limit reached (default 90%), six-phase handoff:

### Phase 1: Generate Summary

Spawn `thread_summary` directive in a separate thread:

```python
summary_result = await thread_directive.execute({
    "directive_name": "rye/agent/threads/thread_summary",
    "model": "fast",
    "inputs": {"transcript_content": transcript_md, "max_summary_tokens": 4000},
    "limit_overrides": {"turns": 3, "spend": 0.02},
})
```

Produces structured summary: Completed Work, Pending Work, Key Decisions, Tool Results.

If summary fails → handoff continues without summary, more trailing messages carried over.

### Phase 2: Build Trailing Messages

Fill remaining token budget with recent messages (most recent first):

```python
remaining_budget = resume_ceiling_tokens - summary_tokens  # e.g., 16000 - 1200
trailing_messages = []
for msg in reversed(messages):
    msg_tokens = len(str(msg.get("content", ""))) // 4
    if trailing_tokens + msg_tokens > remaining_budget:
        break
    trailing_messages.insert(0, msg)
```

Trimmed to start with `user` message (provider requirement).

### Phase 3: Build Resume Messages

```python
resume_messages = [
    {"role": "user", "content": "## Thread Handoff Context\n\n"
     "This thread is a continuation...\n\n" + summary_text},
    {"role": "assistant", "content": "Understood. Continuing."},
    *trailing_messages,
    {"role": "user", "content": "Continue executing the directive."},
]
```

### Phase 4: Spawn New Thread

New thread with same directive and resume messages:

```python
new_result = await thread_directive.execute({
    "directive_name": directive_name,
    "resume_messages": resume_messages,
    "parent_thread_id": original_parent_id,
})
```

Inherits same parent relationship → appears as sibling of original.

### Phase 5: Link Continuation Chain

```python
registry.set_continuation(old_thread_id, new_thread_id)
# old thread: status → "continued", continuation_thread_id → new_thread_id

chain_root_id = registry.get_chain(old_thread_id)[0]["thread_id"]
registry.set_chain_info(new_thread_id, chain_root_id, old_thread_id)
```

### Phase 6: Log Handoff

Recorded in old thread's transcript with new thread ID, summary stats, trailing turn count.

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

Searches `transcript.md` across all threads in the chain.

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

| Aspect       | Automatic Handoff                    | User Resume                          |
|--------------|--------------------------------------|--------------------------------------|
| Trigger      | Context limit (90%+)                 | User calls `resume_thread`           |
| Summary      | Generated by `thread_summary`        | No summary — full transcript rebuilt |
| Context      | Summary + trailing (within ceiling)  | Full reconstruction + new message    |
| Overflow     | Runner will handoff again if needed  | Same — runner handles if too big     |
| Chain        | Old → New linked                     | Old → New linked                     |
| Relationship | Sibling (same parent)                | Sibling (same parent)                |

## Configuration

All continuation settings in `.ai/config/coordination.yaml`:

| Setting                    | Default                               | Description                       |
|----------------------------|---------------------------------------|-----------------------------------|
| `trigger_threshold`        | `0.9`                                 | Context ratio triggering handoff  |
| `resume_ceiling_tokens`    | `16000`                               | Max tokens for summary + trailing |
| `summary_directive`        | `rye/agent/threads/thread_summary`    | Directive for summary generation  |
| `summary_model`            | `fast`                                | Model tier for summaries          |
| `summary_limit_overrides`  | `{turns: 3, spend: 0.02}`            | Limits for summary thread         |
| `summary_max_tokens`       | `4000`                                | Target max summary tokens         |
| `wait_threads.default_timeout` | `600.0`                           | Default wait timeout (seconds)    |
