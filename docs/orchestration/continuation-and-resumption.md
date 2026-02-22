```yaml
id: continuation-and-resumption
title: "Continuation and Resumption"
description: How threads handle context limits, handoffs, and user-driven resumption
category: orchestration
tags: [continuation, resumption, handoff, context-limit]
version: "1.0.0"
```

# Continuation and Resumption

Long-running threads can exceed their LLM's context window. Rather than failing, Rye OS automatically hands off to a new continuation thread — carrying trailing messages and picking up where it left off. Context injection for the new thread is hook-driven. Users can also manually resume completed/errored threads.

## Context Limit Detection

After every turn, the runner estimates how much of the context window has been consumed:

```python
def _check_context_limit(messages, provider, project_path):
    tokens_used = _estimate_message_tokens(messages)  # ~4 chars per token
    context_limit = provider.context_window or 200000
    usage_ratio = tokens_used / context_limit

    # Load threshold from coordination.yaml (default 0.9)
    threshold = coordination_loader.get_continuation_config(project_path)
        .get("trigger_threshold", 0.9)

    if usage_ratio >= threshold:
        return {
            "usage_ratio": usage_ratio,
            "tokens_used": tokens_used,
            "tokens_limit": context_limit,
        }
    return None
```

Token estimation uses `len(content) // 4` — a rough approximation of ~4 characters per token for English text. The threshold defaults to 0.9 (90% of context window), configurable in `.ai/config/coordination.yaml`:

```yaml
coordination:
  continuation:
    trigger_threshold: 0.9
    resume_ceiling_tokens: 16000
```

## Automatic Handoff

When the context limit is reached, `handoff_thread` is triggered automatically by the runner. The handoff has five phases:

### Phase 1: Build Trailing Messages

The token budget is filled with trailing messages from the old conversation (most recent messages first):

```python
trailing_messages = []
for msg in reversed(messages):
    msg_tokens = len(str(msg.get("content", ""))) // 4
    if trailing_tokens + msg_tokens > resume_ceiling:
        break
    trailing_messages.insert(0, msg)
```

The trailing slice is trimmed to start with a `user` message (provider requirement). If no messages fit within the budget, at least the last message is included.

### Phase 2: Build Resume Messages

The resume context is assembled as a new conversation:

```python
resume_messages = [
    # Trailing messages from old conversation
    *trailing_messages,
    # Continuation instruction
    {"role": "user", "content": "Continue executing the directive. "
     "Pick up where the previous thread left off."},
]
```

### Phase 3: Spawn New Thread

A new thread is spawned with the same directive, using the resume messages:

```python
new_result = await thread_directive.execute({
    "directive_name": directive_name,
    "resume_messages": resume_messages,
    "parent_thread_id": original_parent_id,  # same parent chain
    "previous_thread_id": thread_id,         # enables thread_continued hooks
}, project_path)
```

The new thread inherits the same parent relationship, so it appears as a sibling of the original thread in the hierarchy. `previous_thread_id` is passed so the new thread can fire `thread_continued` hooks.

### Phase 4: Link Continuation Chain

The old thread is linked to the new one in the registry:

```python
registry.set_continuation(old_thread_id, new_thread_id)
# old thread: status → "continued", continuation_thread_id → new_thread_id

chain = registry.get_chain(old_thread_id)
chain_root_id = chain[0]["thread_id"]
registry.set_chain_info(new_thread_id, chain_root_id, old_thread_id)
# new thread: chain_root_id → root, continuation_of → old_thread_id
```

### Phase 5: Log Handoff

The handoff is recorded in the old thread's transcript:

```
[thread_handoff] new_thread_id=..., trailing_turns=8
```

The old thread's final status becomes `continued`. The new thread starts with status `running`.

## The Continuation Chain

A continuation chain is a linked list of threads that represent a single logical execution:

```
Thread A (status: continued)
  └→ continuation_thread_id → Thread B (status: continued)
      └→ continuation_thread_id → Thread C (status: completed)
```

Each thread stores:

- `continuation_thread_id` — forward pointer to the next thread in the chain
- `continuation_of` — backward pointer to the previous thread
- `chain_root_id` — the first thread in the chain

### Chain Resolution

`resolve_thread_chain()` follows the chain to find the terminal thread:

```python
def resolve_thread_chain(thread_id, project_path):
    current = thread_id
    visited = set()
    while True:
        if current in visited:
            return current  # cycle — stop
        visited.add(current)
        thread = registry.get_thread(current)
        if not thread or thread.get("status") != "continued":
            return current  # terminal thread
        continuation_id = thread.get("continuation_thread_id")
        if not continuation_id:
            return current
        current = continuation_id
```

The `visited` set prevents infinite loops from corrupted chain data.

**Wait resolution:** When a parent waits on a thread that was continued, `wait_threads` automatically resolves the chain. If you started thread A and it was handed off to B then C, waiting on A's ID returns C's result.

### Viewing the Chain

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "get_chain",
        "thread_id": "my-directive-1739820456"
    }
)
```

Returns:

```json
{
  "success": true,
  "chain_length": 3,
  "chain": [
    {
      "thread_id": "my-directive-1739820456",
      "status": "continued",
      "directive": "my-directive"
    },
    {
      "thread_id": "my-directive-1739820512",
      "status": "continued",
      "directive": "my-directive"
    },
    {
      "thread_id": "my-directive-1739820589",
      "status": "completed",
      "directive": "my-directive"
    }
  ]
}
```

### Searching Across a Chain

Search for content across all transcripts in a continuation chain:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "chain_search",
        "thread_id": "my-directive-1739820456",
        "query": "error.*timeout",
        "search_type": "regex",
        "max_results": 50
    }
)
```

This searches transcript knowledge entries across all threads in the chain — useful for debugging issues that span multiple continuations.

## User-Driven Resumption

Users can resume any completed, errored, or cancelled thread by providing a new message:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "resume_thread",
        "thread_id": "my-directive-1739820456",
        "message": "The API key has been fixed. Please retry the scraping step."
    }
)
```

### Resume Flow

1. **Resolve chain** — If the thread was continued, follow the chain to the terminal thread. The terminal thread is the one that gets resumed.

2. **Validate state** — The thread must be in a terminal state (`completed`, `error`, `cancelled`). Threads that are still `running` or `created` can't be resumed.

3. **Reconstruct messages** — The transcript is parsed back into a message array (user/assistant/tool messages).

4. **Append user message** — The new message is appended to the reconstructed conversation.

5. **Spawn as sibling** — A new thread is spawned with the same directive and the same parent as the original. This preserves the hierarchy — the resumed thread appears alongside the original, not as a child of it.

```python
spawn_params = {
    "directive_name": directive_name,
    "resume_messages": resume_messages,
    "previous_thread_id": resolved_id,
}
if parent_id:
    spawn_params["parent_thread_id"] = parent_id

new_result = await thread_directive.execute(spawn_params, project_path)
```

6. **Link chain** — The original thread gets a `continuation_thread_id` pointing to the new thread. Chain metadata is set on the new thread.

7. **Log in transcript** — A `thread_resumed` event is recorded in the original thread's transcript with the new thread ID, directive name, message preview, and number of reconstructed turns.

### Resume Response

```json
{
  "success": true,
  "resumed": true,
  "old_thread_id": "my-directive-1739820456",
  "new_thread_id": "my-directive-1739820789",
  "original_thread_id": null,
  "resolved_thread_id": "my-directive-1739820456",
  "directive": "my-directive",
  "reconstructed_turns": 12,
  "new_thread_result": { "..." }
}
```

If the original thread_id pointed to an earlier link in a chain (not the terminal), `original_thread_id` shows the ID you passed and `resolved_thread_id` shows the terminal thread that was actually resumed.

### Resume vs Handoff

|                        | Automatic Handoff                                                          | User Resume                                                              |
| ---------------------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| **Trigger**            | Context limit reached (90%+)                                               | User calls `resume_thread`                                               |
| **Summary**            | Hook-driven — directives declare `after_complete` hooks for summarization  | No summary — full transcript reconstructed                               |
| **Context**            | Trailing messages (within token ceiling) + `thread_continued` hook context | Full message reconstruction + new user message                           |
| **When it's too big**  | Runner's context_limit_reached triggers another handoff                    | Same — if reconstructed messages exceed context, the runner will handoff |
| **Chain**              | Old → New linked as continuation                                           | Old → New linked as continuation                                         |
| **Spawn relationship** | Sibling of original (same parent)                                          | Sibling of original (same parent)                                        |

## Example: Hook-Driven Summary on Handoff

Summarization is opt-in. A directive that wants its continuation thread to receive a summary declares three hooks:

```xml
<!-- In the directive's <metadata> block -->
<hooks>
  <!-- 1. Inject dependency context on fresh start -->
  <hook id="inject_api_schema" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.api_thread_id}" />
  </hook>

  <!-- 2. Summarize when the thread completes (or is handed off) -->
  <hook id="summarize_on_complete" event="after_complete">
    <condition path="cost.turns" op="gte" value="1" />
    <action primary="execute" item_type="tool" item_id="rye/agent/threads/thread_summary">
      <param name="thread_id">${thread_id}</param>
    </action>
  </hook>

  <!-- 3. Re-inject the summary + critical context into the continuation -->
  <hook id="reinject_summary" event="thread_continued">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.previous_summary_id}" />
    <description>Summary of what was built before the handoff</description>
  </hook>

  <hook id="reinject_api_schema" event="thread_continued">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.api_thread_id}" />
    <description>API types needed for remaining work</description>
  </hook>
</hooks>
```

The orchestrator passes dependency thread IDs as `inputs` at spawn time:

```python
rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_id": "implement_dashboard",
        "inputs": {
            "api_thread_id": "implement_api/implement_api-1740200200",
        },
    })
```

### What happens when context fills up

1. **Thread A** starts. `thread_started` hooks fire — API schema knowledge injects. Thread builds 3 of 5 views, hits 90% context.

2. **`after_complete` fires** in Thread A's `finally` block. The `summarize_on_complete` hook runs the `thread_summary` tool, which writes a summary as a knowledge entry (e.g., `agent/threads/implement_dashboard/implement_dashboard-1740200300`).

3. **`handoff_thread()` runs.** It packs trailing messages, then spawns a continuation thread with:

```python
spawn_params = {
    "directive_id": "implement_dashboard",
    "resume_messages": resume_messages,
    "previous_thread_id": "implement_dashboard/implement_dashboard-1740200300",
    "inputs": {
        "api_thread_id": "implement_api/implement_api-1740200200",
        "previous_summary_id": "implement_dashboard/implement_dashboard-1740200300",
    },
}
```

4. **Thread B** starts. `thread_continued` hooks fire (not `thread_started`):
   - `reinject_summary` loads Thread A's summary → Thread B knows which 3 views are done and what 2 remain
   - `reinject_api_schema` loads the API schema → Thread B has endpoint definitions and types

Thread B has: trailing turns from Thread A + the summary + API context. It picks up where Thread A left off.

### Without the hooks

If a directive doesn't declare these hooks, the handoff still works — the continuation thread gets the trailing messages and a "Continue executing the directive" instruction. No summary, no extra context. This is fine for simple directives that don't need it.

## Configuration

All continuation behavior is configured in `.ai/config/agent/coordination.yaml`:

| Setting                        | Default  | Description                                                      |
| ------------------------------ | -------- | ---------------------------------------------------------------- |
| `trigger_threshold`            | `0.9`    | Context usage ratio that triggers handoff                        |
| `resume_ceiling_tokens`        | `16000`  | Max tokens for trailing messages in handoff                      |
| `wait_threads.default_timeout` | `600.0`  | Default wait timeout in seconds                                  |
| `transcript_integrity`         | `strict` | Verification mode for transcript signing (`strict` or `lenient`) |

## What's Next

- [Building a Pipeline](./building-a-pipeline.md) — Step-by-step guide to building an orchestrated pipeline
