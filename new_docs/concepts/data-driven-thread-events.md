# Thread Event Configuration Schema

> Data-driven definition of all thread transcript events.
> 
> Location: `.ai/config/thread_events.yaml` (project-level) or 
> `rye/rye/.ai/tools/rye/agent/threads/config/events.yaml` (system defaults)

## Design Principle

All thread behavior is data-driven from YAML configuration. No event types, error classifications, or retry policies are hardcoded in Python. The thread directive tool loads these configurations at runtime and uses them to drive behavior.

## Event Type Definitions

```yaml
# thread_events.yaml
schema_version: "1.0.0"

event_types:
  # Core Lifecycle Events
  thread_started:
    category: lifecycle
    criticality: critical  # Synchronous, blocking write
    description: "Thread execution begins"
    payload_schema:
      type: object
      required: [directive, model, provider]
      properties:
        directive: {type: string}
        model: {type: string}
        provider: {type: string}
        inputs: {type: object}
        thread_mode: 
          type: string
          enum: [single, conversation, channel]
    
  thread_completed:
    category: lifecycle
    criticality: critical
    description: "Thread finished successfully"
    payload_schema:
      type: object
      required: [cost]
      properties:
        cost:
          type: object
          properties:
            turns: {type: integer}
            tokens: {type: integer}
            spend: {type: number}
            duration_seconds: {type: number}
    
  thread_suspended:
    category: lifecycle
    criticality: critical
    description: "Thread suspended awaiting external action"
    payload_schema:
      type: object
      required: [suspend_reason]
      properties:
        suspend_reason:
          type: string
          enum: [limit, error, budget, approval]
        cost: {type: object}
    
  thread_resumed:
    category: lifecycle
    criticality: critical
    description: "Thread resumed from suspension"
    payload_schema:
      type: object
      properties:
        resumed_by: {type: string}
        previous_suspend_reason: {type: string}
    
  thread_cancelled:
    category: lifecycle
    criticality: critical
    description: "Thread cancelled via poison file"
    payload_schema:
      type: object
      properties:
        cancelled_by: {type: string}
        reason: {type: string}

  # Execution Events
  step_start:
    category: execution
    criticality: critical
    description: "LLM turn begins"
    payload_schema:
      type: object
      required: [turn_number]
      properties:
        turn_number: {type: integer}
        
  step_finish:
    category: execution
    criticality: critical
    description: "LLM turn ends"
    payload_schema:
      type: object
      required: [cost, tokens, finish_reason]
      properties:
        cost: {type: number}
        tokens:
          type: object
          properties:
            input_tokens: {type: integer}
            output_tokens: {type: integer}
        finish_reason:
          type: string
          enum: [end_turn, tool_use, limit_exceeded, error]

  # Cognition Events (replacing user_message/assistant_text)
  cognition_in:
    category: cognition
    criticality: critical
    description: "Context/prompt sent to LLM"
    payload_schema:
      type: object
      required: [text, role]
      properties:
        text: {type: string}
        role:
          type: string
          enum: [system, user, developer]
    
  cognition_out:
    category: cognition
    criticality: critical
    description: "Complete or partial LLM-generated output"
    emit_on_error: true  # Always emit partial text even if stream fails
    payload_schema:
      type: object
      required: [text]
      properties:
        text: {type: string}
        model: {type: string}
        is_partial:
          type: boolean
          description: "True if stream was interrupted before completion"
          default: false
        truncated:
          type: boolean
          description: "True if text was cut off due to error/limit"
          default: false
        error:
          type: string
          description: "Error message if stream failed"
        completion_percentage:
          type: number
          description: "Estimated completion percentage (0-100)"
          minimum: 0
          maximum: 100
    
  cognition_out_delta:
    category: cognition
    criticality: droppable  # Async, fire-and-forget
    description: "Streaming text chunk (optional)"
    payload_schema:
      type: object
      required: [text, chunk_index]
      properties:
        text: {type: string}
        chunk_index: {type: integer}
        is_final: {type: boolean}
    emit_config:
      async: true
      throttle: 1s
      condition: provider_config.stream.enabled
    
  cognition_reasoning:
    category: cognition
    criticality: droppable
    description: "Reasoning/thinking block (may be partial on error)"
    emit_on_error: true  # Accumulate and emit partial reasoning on interruption
    payload_schema:
      type: object
      required: [text]
      properties:
        text: {type: string}
        is_partial:
          type: boolean
          description: "True if reasoning was cut off by interruption"
          default: false
        was_interrupted:
          type: boolean
          description: "True if stream failed before reasoning completed"
          default: false
    emit_config:
      async: true
      accumulate_on_error: true  # Buffer reasoning chunks, emit accumulated on error
      condition: provider_config.stream.enabled

  # Tool Events
  tool_call_start:
    category: tool
    criticality: critical
    description: "Tool execution begins"
    payload_schema:
      type: object
      required: [tool, call_id, input]
      properties:
        tool: {type: string}
        call_id: {type: string}
        input: {type: object}
    
  tool_call_progress:
    category: tool
    criticality: droppable
    description: "Progress update for long-running tools"
    payload_schema:
      type: object
      required: [call_id, progress]
      properties:
        call_id: {type: string}
        progress:
          type: number
          minimum: 0
          maximum: 100
        message: {type: string}
    emit_config:
      async: true
      throttle: 1s
      milestones: [0, 25, 50, 75, 100]
    
  tool_call_result:
    category: tool
    criticality: critical
    description: "Tool execution completes"
    payload_schema:
      type: object
      required: [call_id, output]
      properties:
        call_id: {type: string}
        output: {type: string}
        error: {type: string}
        duration_ms: {type: integer}

  # Error & Recovery Events
  error_classified:
    category: error
    criticality: critical
    description: "Error classified for retry/fail decision"
    payload_schema:
      type: object
      required: [error_code, category]
      properties:
        error_code: {type: string}
        category:
          type: string
          enum: [transient, permanent, rate_limited, quota, limit_hit, budget, cancelled]
        retryable: {type: boolean}
        metadata: {type: object}
    
  retry_succeeded:
    category: error
    criticality: critical
    description: "Transient error resolved after retry"
    payload_schema:
      type: object
      required: [original_error, retry_count]
      properties:
        original_error: {type: string}
        retry_count: {type: integer}
        total_delay_ms: {type: integer}

  limit_escalation_requested:
    category: error
    criticality: critical
    description: "Limit hit, escalation sent for approval"
    payload_schema:
      type: object
      required: [limit_code, current_value, proposed_max]
      properties:
        limit_code:
          type: string
          enum: [turns_exceeded, tokens_exceeded, spend_exceeded, spawns_exceeded, duration_exceeded]
        current_value: {type: number}
        current_max: {type: number}
        proposed_max: {type: number}
        message: {type: string}
        approval_request_id: {type: string}

  # Orchestration Events
  child_thread_started:
    category: orchestration
    criticality: critical
    description: "Child thread spawned"
    payload_schema:
      type: object
      required: [child_thread_id, child_directive]
      properties:
        child_thread_id: {type: string}
        child_directive: {type: string}
        parent_thread_id: {type: string}
    
  child_thread_failed:
    category: orchestration
    criticality: critical
    description: "Child thread completed with error"
    payload_schema:
      type: object
      required: [child_thread_id, error]
      properties:
        child_thread_id: {type: string}
        error: {type: string}

  # Context Management Events
  context_compaction_start:
    category: compaction
    criticality: critical
    description: "Compaction summarization begins"
    payload_schema:
      type: object
      properties:
        triggered_by: {type: string}
        pressure_ratio: {type: number}
    
  context_compaction_end:
    category: compaction
    criticality: critical
    description: "Compaction summarization ends"
    payload_schema:
      type: object
      properties:
        summary: {type: string}
        prune_before_turn: {type: integer}

# Criticality Levels
criticality_levels:
  critical:
    description: "Synchronous, blocking write. Thread waits for completion."
    durability: guaranteed
    async: false
    
  droppable:
    description: "Fire-and-forget async emission. Optional."
    durability: best_effort
    async: true
    fallback: drop

# Event Categories
categories:
  lifecycle: "Thread start/stop/resume events"
  execution: "LLM turn events"
  cognition: "Input/output/reasoning events"
  tool: "Tool call events"
  error: "Error handling events"
  orchestration: "Child thread events"
  compaction: "Context window management"

# Error Handling & Partial Cognition

## Stream Interruption Handling

When a streaming LLM call is interrupted (network error, rate limit, cancellation), the system handles partial data as follows:

### 1. Partial Cognition Out (Always Emitted)

Even on error, `cognition_out` is emitted with accumulated text:

```python
try:
    async for chunk in _stream_llm():
        full_text += chunk
        emit_droppable(transcript, thread_id, "cognition_out_delta", {"text": chunk})
    
    # Success - emit complete cognition
    transcript.write_event(thread_id, "cognition_out", {
        "text": full_text,
        "is_partial": False,
    })
    
except Exception as e:
    # Failure - emit partial cognition for resume
    transcript.write_event(thread_id, "cognition_out", {
        "text": full_text,  # Accumulated so far
        "is_partial": True,
        "truncated": True,
        "error": str(e),
        "completion_percentage": estimate_completion(full_text),
    })
    
    # Also emit partial reasoning if any was accumulated
    if reasoning_buffer:
        transcript.write_event(thread_id, "cognition_reasoning", {
            "text": reasoning_buffer,
            "is_partial": True,
            "was_interrupted": True,
        })
```

### 2. Partial Reasoning Preserved

Reasoning blocks (thinking/CoT) are also accumulated and emitted on error:

```python
reasoning_buffer = ""

async for chunk in _stream_llm():
    if is_reasoning_chunk(chunk):
        reasoning_buffer += chunk.content
        # Droppable event for real-time UI
        emit_droppable(transcript, thread_id, "cognition_reasoning_delta", {
            "text": chunk.content,
        })
    else:
        full_text += chunk.content

# On error - emit accumulated reasoning (even if droppable normally)
if reasoning_buffer:
    transcript.write_event(thread_id, "cognition_reasoning", {
        "text": reasoning_buffer,
        "is_partial": True,  # Reasoning was cut off
        "was_interrupted": True,
    })
```

**Why preserve reasoning:**
- Shows the LLM's thought process that led to the partial output
- Useful for debugging why the response was heading in a certain direction
- Can be included in context on resume so LLM doesn't re-think from scratch

### 3. Completed Tools Preserved

Tools that finished before the error are preserved:

```python
# Tool results already in transcript:
# - tool_call_start
# - tool_call_result (with output)
# - cognition_out (partial with is_partial: true)

# On retry:
# - Partial cognition included in context
# - Completed tool results included in context
# - Incomplete tool calls discarded (never had tool_call_result)
```

### 3. Replay from Partial State

When resuming from a partial `cognition_out`:

```python
def reconstruct_from_transcript(events: List[Dict]) -> List[Dict]:
    messages = []
    
    for event in events:
        if event["type"] == "cognition_out":
            content = event["text"]
            
            if event.get("is_partial"):
                # Partial cognition - add note about interruption
                content += f"\n\n[Stream interrupted: {event.get('error', 'Unknown error')}"
                content += f" - {event.get('completion_percentage', '?')}% complete]"
            
            messages.append({
                "role": "assistant",
                "content": content,
                "is_partial": event.get("is_partial", False),
            })
    
    return messages
```

### 4. User Experience

The user sees the partial context:

```
Assistant: Let me analyze the code structure...
[Stream interrupted: Connection reset - 65% complete]

User: Continue from where you left off

Assistant: [Resuming] ...so the bug is in line 42.
```

## Event Configuration: emit_on_error

The `emit_on_error` flag controls whether an event is emitted when an error occurs:

```yaml
event_types:
  cognition_out:
    emit_on_error: true  # Always emit partial data
    
  thread_completed:
    emit_on_error: false  # Only emitted on success
    
  thread_error:
    emit_on_error: true   # Always emitted on error
```

**Benefits of emitting partial cognition:**
- Context is preserved even on failure
- User sees what the LLM was thinking before interruption
- Retry can continue from partial state (not restart from scratch)
- Audit trail shows full execution history including partial outputs
