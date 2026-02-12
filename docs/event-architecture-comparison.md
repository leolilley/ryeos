# Event-Driven Architecture Comparison: Rye OS vs OpenClaw

## Executive Summary

This document compares the event-driven architecture patterns between **Rye OS** (as defined in our design documents) and **OpenClaw** (as implemented in `/home/leo/projects/openclaw`). The goal is to identify patterns from OpenClaw that could enhance our implementation while validating that our asyncio-based coordination model is architecturally superior.

---

## Architecture Overview

### OpenClaw: Event-Driven Dispatcher Pattern

OpenClaw uses a **central event dispatcher** with type-based routing. All events flow through a single handler function that switches on event type:

```typescript
// From: /home/leo/projects/openclaw/src/agents/pi-embedded-subscribe.handlers.ts
export function createEmbeddedPiSessionEventHandler(
  ctx: EmbeddedPiSubscribeContext,
) {
  return (evt: EmbeddedPiSubscribeEvent) => {
    switch (evt.type) {
      case "message_start":
        handleMessageStart(ctx, evt);
        return;
      case "message_update":
        handleMessageUpdate(ctx, evt);
        return;
      case "tool_execution_start":
        // Fire-and-forget async handler
        handleToolExecutionStart(ctx, evt).catch((err) => {
          ctx.log.debug(`handler failed: ${String(err)}`);
        });
        return;
      case "tool_execution_end":
        handleToolExecutionEnd(ctx, evt);
        return;
      // ... additional cases
    }
  };
}
```

**Key Characteristics:**

- All events flow through a single dispatcher
- Handlers can be fire-and-forget (non-blocking)
- Context object passed to all handlers
- Event types: `message_start`, `message_update`, `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `agent_start`, `agent_end`, `auto_compaction_start`, `auto_compaction_end`

### Rye OS: Separation of Concerns Pattern

Rye OS uses a **dual-path architecture**:

1. **Coordination Path** (`asyncio.Event`): Instant, in-process signals for thread completion
2. **Audit Path** (Transcript JSONL): Durable, cross-process records for replay and analysis

```python
# From: thread-orchestration-internals.md
# Coordination via asyncio.Event
_completion_events: Dict[str, asyncio.Event] = {}

def signal_completion(thread_id: str) -> None:
    """Signal that a thread has reached a terminal state."""
    event = _completion_events.get(thread_id)
    if event:
        event.set()  # Wake up wait_threads immediately

# Audit via transcript events
transcript.write_event(thread_id, "tool_call_result", {
    "call_id": call_id,
    "output": result,
    "duration_ms": duration_ms,
})
```

**Key Characteristics:**

- Coordination signals flow through `asyncio.Event` (push-based, zero polling)
- Audit records flow through transcript JSONL (durable, post-hoc analysis)
- Separation prevents mixing notification with coordination
- Events are emitted at strategic checkpoints

---

## Detailed Pattern Comparison

### 1. Event Granularity

#### OpenClaw: Fine-Grained Streaming Events

OpenClaw emits events at a granular level, particularly for streaming:

```typescript
// From: pi-embedded-subscribe.ts streaming handler
const state: EmbeddedPiSubscribeState = {
  deltaBuffer: "",
  blockBuffer: "",
  blockState: {
    thinking: false,
    final: false,
    inlineCode: createInlineCodeState(),
  },
  // ... extensive streaming state
};

// Events emitted:
// - message_start: Beginning of LLM response
// - message_update: Each text_delta chunk
// - tool_execution_update: Progress during tool execution
// - message_end: Complete response received
```

**Pattern:** Real-time streaming enables immediate UI feedback and early tool call detection.

#### Rye OS: Checkpoint-Based Events

Rye OS emits events at completion boundaries:

```python
# From: thread_directive.py
# Current event emission points:
- "user_message": Start of conversation
- "step_start": Beginning of LLM turn
- "assistant_text": Complete assistant response
- "assistant_reasoning": Complete reasoning block
- "tool_call_start": Tool execution begins
- "tool_call_result": Tool execution completes
- "step_finish": End of LLM turn
```

**Gap:** No `text_delta` events for streaming responses.

**Recommendation:** Add optional streaming support via `assistant_text_delta` events:

```python
async def _call_llm_streaming(...):
    async for chunk in llm_provider.stream():
        if chunk.type == "text_delta":
            # Optional: emit delta for real-time UI
            transcript.write_event(thread_id, "assistant_text_delta", {
                "delta": chunk.text,
            })
            # Accumulate for final assistant_text event
            accumulated_text += chunk.text
```

### 2. Tool Execution Lifecycle

#### OpenClaw: Three-Phase Tool Events

```typescript
// From: pi-embedded-subscribe.handlers.tools.ts
case "tool_execution_start":
  // Begin tool execution, show typing indicator
  handleToolExecutionStart(ctx, evt);
  break;
case "tool_execution_update":
  // Progress updates for long-running tools
  handleToolExecutionUpdate(ctx, evt);
  break;
case "tool_execution_end":
  // Tool completed, process results
  handleToolExecutionEnd(ctx, evt);
  break;
```

**Pattern:** Enables progress reporting for long-running operations.

#### Rye OS: Two-Phase Tool Events

```python
# Current implementation
transcript.write_event(thread_id, "tool_call_start", {
    "tool": tc["name"],
    "call_id": call_id,
    "input": tc["input"],
})

# ... execute tool ...

transcript.write_event(thread_id, "tool_call_result", {
    "call_id": call_id,
    "output": result,
    "error": error,
    "duration_ms": duration_ms,
})
```

**Gap:** No progress updates during tool execution.

**Recommendation:** Add optional `tool_call_progress` for tools that support it:

```python
# If tool supports progress reporting:
for progress in tool.execute_with_progress():
    transcript.write_event(thread_id, "tool_call_progress", {
        "call_id": call_id,
        "progress": progress.percentage,
        "message": progress.message,
    })
```

### 3. Handler Execution Model

#### OpenClaw: Fire-and-Forget Async

```typescript
// Non-blocking handler with error isolation
handleToolExecutionStart(ctx, evt).catch((err) => {
  ctx.log.debug(`tool_execution_start handler failed: ${String(err)}`);
});
// Main loop continues immediately
```

**Pattern:** Handlers don't block the main execution flow. Errors in handlers don't crash the agent.

#### Rye OS: Blocking Event Emission

```python
# Current implementation
try:
    transcript.write_event(thread_id, "tool_call_start", {...})
except Exception as e:
    logger.warning(f"Failed to write tool_call_start event: {e}")
```

**Gap:** All event writes are blocking (though typically fast with file I/O).

**Recommendation:** For non-critical events (metrics, progress), consider async fire-and-forget:

```python
# For non-critical events only
def emit_event_async(thread_id: str, event_type: str, data: dict):
    """Fire-and-forget event emission for non-critical events."""
    asyncio.create_task(_write_event_safe(thread_id, event_type, data))

async def _write_event_safe(thread_id, event_type, data):
    try:
        transcript.write_event(thread_id, event_type, data)
    except Exception as e:
        logger.debug(f"Async event write failed: {e}")
```

### 4. State Management

#### OpenClaw: Centralized State Object

```typescript
// From: pi-embedded-subscribe.ts
const state: EmbeddedPiSubscribeState = {
  assistantTexts: [],
  toolMetas: [],
  toolMetaById: new Map(),
  deltaBuffer: "",
  blockBuffer: "",
  blockState: {
    thinking: false,
    final: false,
    inlineCode: createInlineCodeState(),
  },
  // ... extensive mutable state
};

// Handlers receive context with state
function handleMessageUpdate(ctx: EmbeddedPiSubscribeContext, evt) {
  ctx.state.deltaBuffer += evt.delta;
  // ... process delta
}
```

**Pattern:** Single mutable state object shared across all handlers.

#### Rye OS: Distributed State

```python
# Harness tracks cost/limits
harness: SafetyHarness

# Transcript tracks event history
transcript: TranscriptWriter

# Registry tracks thread metadata
registry: ThreadRegistry

# Budget ledger tracks hierarchical spend
ledger: BudgetLedger

# Active tasks tracked module-level
_active_tasks: Dict[str, asyncio.Task]
_completion_events: Dict[str, asyncio.Event]
```

**Advantage:** Clear separation of concerns. Each component owns its state.

**Recommendation:** Maintain current separation. No need to consolidate into a monolithic state object.

### 5. Lifecycle Events

#### OpenClaw: Explicit Agent Lifecycle

```typescript
case "agent_start":
  handleAgentStart(ctx);
  break;
case "auto_compaction_start":
  handleAutoCompactionStart(ctx);
  break;
case "auto_compaction_end":
  handleAutoCompactionEnd(ctx, evt);
  break;
case "agent_end":
  handleAgentEnd(ctx);
  break;
```

**Pattern:** Explicit lifecycle events for agent session and compaction.

#### Rye OS: Implicit via Thread Status

Rye OS tracks lifecycle through:

- Thread status in registry: `running` → `suspended` → `completed`/`error`
- Transcript events: `step_start`, `step_finish`
- State persistence: `state.json` at checkpoints

**Gap:** No explicit `agent_start`/`agent_end` or compaction events.

**Recommendation:** Add lifecycle events to transcript:

```python
# At thread start
transcript.write_event(thread_id, "thread_started", {
    "directive": directive_name,
    "model": model_id,
    "limits": harness.limits,
})

# At compaction (from thread-resilience-and-recovery.md)
transcript.write_event(thread_id, "context_compaction_start", {
    "reason": "context_window",
    "tokens_before": token_count,
})

# ... perform compaction ...

transcript.write_event(thread_id, "context_compaction_end", {
    "tokens_after": new_token_count,
    "summarized_turns": num_turns,
})

# At thread end (success/error/cancelled)
transcript.write_event(thread_id, "thread_completed", {
    "status": final_status,  # completed, error, cancelled
    "cost": harness.cost.to_dict(),
})
```

### 6. Coordination Mechanisms

#### OpenClaw: Complex Polling/State Machine

OpenClaw's coordination is not clearly visible in the code analyzed, but appears to rely on:

- State machine transitions
- Provider-specific stream handling
- Retry loops with backoff

#### Rye OS: asyncio.Event (Superior)

```python
# From: thread-orchestration-internals.md
# Child thread signals completion
def signal_completion(thread_id: str) -> None:
    """Called from thread_directive.execute()'s finally block."""
    event = _completion_events.get(thread_id)
    if event:
        event.set()

# Parent awaits completion (zero polling)
async def wait_for_thread(tid: str, event: asyncio.Event):
    await event.wait()
    task = get_task(tid)
    if task and task.done():
        return tid, {
            "status": task.result().get("status", "completed"),
            "cost": task.result().get("cost", {}),
        }
```

**Verdict:** Rye OS's asyncio.Event pattern is architecturally superior to polling-based approaches.

**Advantages:**

- Zero latency (immediate wake on completion)
- Zero token cost (no LLM turns spent polling)
- No race conditions (event set in `finally` block)
- Works across success/error/cancellation/suspension

---

## OpenClaw Patterns Worth Adopting

### 1. Streaming Delta Support

**Use Case:** Real-time UI updates during long LLM responses.

**Implementation:**

```python
# Add to transcript schema:
event_types = [
    # ... existing events
    "assistant_text_delta",  # NEW: streaming text chunks
    "tool_call_progress",    # NEW: tool execution progress
]

# Emit conditionally based on provider capability
if provider.supports_streaming:
    async for chunk in llm.stream():
        if chunk.type == "text_delta":
            transcript.write_event(thread_id, "assistant_text_delta", {
                "delta": chunk.text,
            })
```

### 2. Fire-and-Forget Non-Critical Handlers

**Use Case:** Metrics, logging, notifications that shouldn't block main flow.

**Implementation:**

```python
# For metrics/progress only (NOT for coordination)
def emit_async(event_type: str, data: dict):
    asyncio.create_task(_safe_emit(event_type, data))

async def _safe_emit(event_type, data):
    try:
        metrics.record(event_type, data)
    except Exception:
        pass  # Silent failure for metrics
```

### 3. Explicit Compaction Events

**Use Case:** Audit trail for context window management.

**Implementation:**

```python
# From thread-resilience-and-recovery.md compaction logic
transcript.write_event(thread_id, "context_compaction_start", {...})
# ... summarize ...
transcript.write_event(thread_id, "context_compaction_end", {...})
```

### 4. Tool Execution Granularity

**Use Case:** Long-running tools (builds, tests, data processing) need progress indicators.

**Implementation:**

```python
# Optional progress callback for tools
def execute_with_progress(tool_input, on_progress=None):
    for i, item in enumerate(items):
        process(item)
        if on_progress:
            on_progress(i / len(items), f"Processed {i}/{len(items)}")
```

---

## Rye OS Patterns That Are Superior

### 1. Coordination/Audit Separation

**Rye OS Insight:**

> "Coordination signals flow through `asyncio.Event` (instant, in-process). Audit records flow through transcript JSONL (durable, cross-process). Mixing the two creates a system where the notification exists but nobody reads it during execution."

**Why Superior:**

- Clear separation of concerns
- No polling overhead
- Durable audit trail independent of coordination state
- Works across process restarts

### 2. Hierarchical Capability Attenuation

**Rye OS Pattern:**

```python
# Child attenuates parent token to intersection with own permissions
child_token = parent_token.intersect(child_permissions)
```

**Why Superior:**

- Security boundary enforced at runtime
- Capability narrowing is explicit and auditable
- Prevents privilege escalation

### 3. Budget Ledger with Reservations

**Rye OS Pattern:**

```python
# Reserve budget before spawning child
if not ledger.reserve(parent_id, child_id, child_max_spend):
    return {"status": "budget_exceeded"}

# Report actual on completion
ledger.report_actual(child_id, actual_spend)
```

**Why Superior:**

- Prevents budget overruns before they happen
- Atomic reservations via SQLite transactions
- Hierarchical enforcement across thread tree

### 4. Checkpoint-Based State Persistence

**Rye OS Pattern:**

```python
# Save at every checkpoint (pre-LLM, post-LLM, post-tools)
save_state(thread_id, harness, project_path)
```

**Why Superior:**

- Crash recovery from any point
- Resumable after suspension
- Audit trail of execution state

---

## Hybrid Recommendations

### Recommended Event Schema

```yaml
# Core coordination events (required)
- thread_started
- step_start
- assistant_text # Full text (always emitted)
- assistant_text_delta # Streaming chunks (optional)
- assistant_reasoning
- tool_call_start
- tool_call_progress # Optional for long tools
- tool_call_result
- step_finish
- thread_completed

# Lifecycle events (for audit/debugging)
- thread_suspended
- thread_resumed
- context_compaction_start
- context_compaction_end
- child_thread_started
- child_thread_completed

# Error/Recovery events
- error_classified
- retry_succeeded
- limit_escalation_requested
```

### Recommended Handler Patterns

```python
# For coordination (blocking, reliable)
transcript.write_event(thread_id, "tool_call_start", {...})

# For progress (optional, async)
emit_async("tool_call_progress", {"percent": 50})

# For metrics (fire-and-forget)
emit_async("metric_tool_duration", {"tool": name, "ms": duration_ms})
```

---

## Conclusion

### What to Adopt from OpenClaw

1. **Streaming deltas** for real-time UI updates (optional enhancement)
2. **Fire-and-forget handlers** for non-critical events
3. **Explicit compaction lifecycle events** for audit trail
4. **Tool progress updates** for long-running operations

### What Makes Rye OS Superior

1. **asyncio.Event coordination** - Zero polling, instant notification
2. **Separation of coordination and audit** - Clean architecture
3. **Capability-based security** - Hierarchical attenuation
4. **Budget reservations** - Prevent overruns, not just detect them
5. **Checkpoint persistence** - Crash recovery at any point

### Final Assessment

Rye OS's architecture is **more principled** than OpenClaw's. The key insight—separating coordination signals (asyncio.Event) from audit records (transcript JSONL)—is architecturally sound and avoids the complexity of mixed-mode event handling.

OpenClaw's patterns are **pragmatic** and optimized for real-time UX, but at the cost of complexity. Rye OS should adopt the UX-friendly patterns (streaming, progress) while maintaining its superior coordination model.

---

## References

### OpenClaw Source Files

- `/home/leo/projects/openclaw/src/agents/pi-embedded-subscribe.ts` - Main streaming handler
- `/home/leo/projects/openclaw/src/agents/pi-embedded-subscribe.handlers.ts` - Event dispatcher
- `/home/leo/projects/openclaw/src/agents/pi-embedded-runner/run.ts` - Agent execution loop

### Rye OS Design Documents

- `/home/leo/projects/rye-os/new_docs/concepts/thread-orchestration-internals.md`
- `/home/leo/projects/rye-os/new_docs/concepts/thread-resilience-and-recovery.md`
- `/home/leo/projects/rye-os/new_docs/concepts/bundler-tool-architecture.md`

### Current Implementation

- `/home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/thread_directive.py`
- `/home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/safety_harness.py`
