# Streaming Inline Tool Execution

> Execute tools as soon as their definition arrives in the LLM stream, not after the full response completes.
>
> **Location:** `rye/rye/.ai/tools/rye/agent/threads/`

## Overview

Replaces the "batch-after-response" model with opencode-style streaming inline execution. Tools execute **during** LLM response generation, reducing latency by starting work immediately.

**Configuration:** All streaming behavior is data-driven from YAML. See [data-driven-streaming-config.md](data-driven-streaming-config.md) for HTTP primitive settings, sink configurations, and event extraction rules.

## HTTP Primitive Streaming Architecture

The streaming pipeline uses the HTTP primitive's built-in SSE (Server-Sent Events) support with configurable sinks:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         Streaming Pipeline                              │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Anthropic/OpenAI API                                                    │
│         │                                                               │
│         ▼                                                               │
│  ┌─────────────────┐                                                    │
│  │ HTTP Primitive  │  mode: "stream"                                     │
│  │ (lilux)         │  ┌─► SSE Parser (data: lines)                      │
│  └─────────────────┘  │                                                 │
│         │             │                                                 │
│         ▼             │                                                 │
│  ┌─────────────────┐  │    ┌──────────────┐    ┌──────────────────┐    │
│  │    SINKS        │◄─┘    │ WebSocket    │    │ File             │    │
│  │  (fan-out)      ├──────►│ Sink         │    │ Sink             │    │
│  └─────────────────┘       │ (UI real-time)│    │ (audit log)      │    │
│         │                  └──────────────┘    └──────────────────┘    │
│         │                                                               │
│         ▼                                                               │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Thread Directive                                               │   │
│  │  ┌─► StreamingToolParser ──► Tool calls                         │   │
│  │  ├─► cognition_out_delta ──► Transcript                        │   │
│  │  └─► cognition_reasoning ──► Transcript                        │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### How It Works

1. **HTTP Primitive** streams SSE from LLM provider (Anthropic/OpenAI)
2. **Sinks** receive raw SSE events via fan-out:
   - `WebSocketSink` → UI for real-time display
   - `FileSink` → Audit log `.ai/threads/{id}/stream.jsonl`
3. **Thread Directive** receives events via `ReturnSink`, parses them:
   - Text chunks → `cognition_out_delta` events (droppable)
   - Tool definitions → `StreamingToolParser` → execute immediately
   - Reasoning blocks → `cognition_reasoning` events (droppable)
4. **Final event**: After stream completes → `cognition_out` (critical, full text)

### Provider Configuration

```yaml
# rye/rye/.ai/tools/rye/agent/threads/config/providers/anthropic.yaml
provider:
  name: "anthropic"
  api_url: "https://api.anthropic.com/v1/messages"

stream:
  enabled: true
  format: "sse" # Server-Sent Events

  # Sinks receive raw SSE events
  sinks:
    - type: websocket
      url: "${THREAD_UI_WEBSOCKET_URL:-ws://localhost:8080/events}"
      reconnect_attempts: 3
      buffer_on_disconnect: true

    - type: file
      path: ".ai/threads/{thread_id}/sse-stream.jsonl"
      format: "jsonl"

  # What to extract from SSE events
  extract:
    text: "$.delta.text" # JSONPath to text content
    tool_calls: "$.content_blocks[?type=='tool_use']"
    reasoning: "$.content_blocks[?type=='thinking']"
    stop_reason: "$.stop_reason"
```

### Sink Types

| Sink        | Purpose                      | Critical? | Config             |
| ----------- | ---------------------------- | --------- | ------------------ |
| `websocket` | Real-time UI updates         | Droppable | reconnect, buffer  |
| `file`      | Audit trail                  | Droppable | rotation, max_size |
| `return`    | Thread directive consumption | Required  | buffer size        |

### Thread Directive Integration

```python
async def _stream_llm_with_sinks(provider_config, messages, thread_id):
    """Stream LLM with configured sinks."""

    # Build sinks from provider config
    sinks = []
    for sink_config in provider_config.stream.sinks:
        if sink_config.type == "websocket":
            sinks.append(WebSocketSink(**sink_config))
        elif sink_config.type == "file":
            sinks.append(FileSink(**sink_config))

    # Always add ReturnSink for thread directive
    return_sink = ReturnSink()
    sinks.append(return_sink)

    # Call HTTP primitive in stream mode
    result = await http_primitive.execute(
        config={
            "url": provider_config.api_url,
            "method": "POST",
            "headers": {"Authorization": f"Bearer {api_key}"},
            "body": {"messages": messages, "stream": True},
        },
        params={
            "mode": "stream",  # Enable streaming
            "__sinks": sinks,  # Fan-out destinations
        }
    )

    # Return sink has all events for parsing
    return return_sink.get_events()
```

## Architecture

### Key Differences from Batch Model

| Aspect                 | Batch (Old)                    | Streaming (New)                    |
| ---------------------- | ------------------------------ | ---------------------------------- |
| Tool execution trigger | After complete LLM response    | As soon as tool definition arrives |
| Latency                | Wait for full response + tools | Tools run while LLM continues      |
| Parallelism            | All tools together             | Accumulate batch, dispatch grouped |
| Transcript             | Sequential turn → tools        | Interleaved: delta → tool → delta  |

### Components

```
┌─────────────────────────────────────────────────────────────┐
│  _run_tool_use_loop_streaming()                             │
├─────────────────────────────────────────────────────────────┤
│  StreamingToolParser ──► accumulates partial tool calls    │
│  Pending Tool Buffer ──► batches tools (100ms or 5 ready)   │
│  Parallel Dispatcher ──► executes grouped by item_id        │
│  Event Emitter ──► writes interleaved transcript events    │
└─────────────────────────────────────────────────────────────┘
```

## StreamingToolParser

Accumulates partial tool calls from LLM stream chunks. Supports both XML (`<tool_use>`) and JSON formats.

```python
class StreamingToolParser:
    """Parse tool calls from streaming LLM chunks.

    Yields events as tool definitions complete mid-stream.
    """

    def __init__(self, format: str = "xml"):
        self._format = format  # "xml" or "json"
        self._partial_tools: Dict[str, str] = {}
        self._text_buffer: List[str] = []
        self._in_tool = False
        self._current_tool_id: Optional[str] = None
        self._full_text: str = ""

    def feed_chunk(self, chunk: str) -> Iterator[Tuple[str, Any]]:
        """Process a stream chunk.

        Yields:
            ('text', str) - text delta for UI/transcript
            ('tool_complete', dict) - complete tool call ready to execute
            ('error', str) - parse error
        """
        if self._format == "xml":
            yield from self._parse_xml_chunk(chunk)
        else:
            yield from self._parse_json_chunk(chunk)

    def get_full_text(self) -> str:
        """Return complete accumulated text."""
        return self._full_text

    def _parse_xml_chunk(self, chunk: str) -> Iterator[Tuple[str, Any]]:
        """Parse XML-style <tool_use>...</tool_use> tags."""
        # State machine for tag matching
        # Accumulate text between tools as 'text' events
        # When </tool_use> found, parse accumulated XML, yield 'tool_complete'
        pass

    def _parse_json_chunk(self, chunk: str) -> Iterator[Tuple[str, Any]]:
        """Parse JSON-style tool call objects."""
        # Incremental JSON parsing
        # Handle both single objects and arrays
        # Yield 'tool_complete' when closing brace/bracket matched
        pass
```

## Streaming Loop with Batching

Tools don't execute one-by-one. They accumulate for efficient parallel dispatch:

```python
async def _run_tool_use_loop_streaming(
    ...
    thread_id: str = "",
) -> Dict[str, Any]:
    parser = StreamingToolParser(format=provider_config.format)
    pending_tools: List[Dict] = []
    batch_timer: Optional[asyncio.Task] = None
    completed_tools: List[Dict] = []

    async def execute_pending_batch():
        """Execute accumulated tools in parallel."""
        nonlocal pending_tools
        if not pending_tools:
            return

        # Emit tool_call_start for each
        for tool in pending_tools:
            transcript.write_event(thread_id, "tool_call_start", {
                "tool": tool["name"],
                "call_id": tool["id"],
                "input": tool["input"],
            })

        # Execute grouped by item_id
        results = await _dispatch_tool_calls_parallel(
            pending_tools, tool_map, project_path, ...
        )

        # Emit tool_call_result for each
        for tool, result in zip(pending_tools, results):
            transcript.write_event(thread_id, "tool_call_result", {
                "call_id": tool["id"],
                "output": result.get("output"),
                "error": result.get("error"),
            })

        completed_tools.extend(results)
        pending_tools.clear()

    try:
        async for chunk in _stream_llm_with_retry(...):
            for event_type, data in parser.feed_chunk(chunk):
                if event_type == 'text':
                    # Emit droppable delta for UI
                    emit_droppable(transcript, thread_id, "cognition_out_delta", {
                        "text": data,
                        "turn": turn,
                    })

                elif event_type == 'tool_complete':
                    pending_tools.append(data)

                    # Cancel existing timer
                    if batch_timer and not batch_timer.done():
                        batch_timer.cancel()

                    # Execute immediately if batch size reached
                    if len(pending_tools) >= TOOL_BATCH_SIZE:
                        await execute_pending_batch()
                    else:
                        # Set timer for delayed execution
                        batch_timer = asyncio.create_task(
                            asyncio.sleep(TOOL_BATCH_DELAY)
                        )
                        batch_timer.add_done_callback(
                            lambda _: asyncio.create_task(execute_pending_batch())
                        )

                elif event_type == 'error':
                    logger.error(f"Tool parse error: {data}")

        # Stream finished - execute any remaining tools
        if batch_timer and not batch_timer.done():
            batch_timer.cancel()
        await execute_pending_batch()

        # Emit final cognition_out (always after stream)
        transcript.write_event(thread_id, "cognition_out", {
            "text": parser.get_full_text(),
            "turn": turn,
            "tool_count": len(completed_tools),
        })

        return {
            "success": True,
            "text": parser.get_full_text(),
            "tools": completed_tools,
        }

    except Exception as e:
        # Stream failed - preserve any completed tools
        return {
            "success": False,
            "error": str(e),
            "partial_tools": completed_tools,
        }
```

## Configuration Constants

```yaml
# rye/rye/.ai/tools/rye/agent/threads/config/streaming.yaml
schema_version: "1.0.0"

streaming_config:
  # Tool batching settings
  tool_batch_size: 5 # Execute immediately when this many tools ready
  tool_batch_delay: 0.1 # Seconds to wait for more tools before executing

  # Parser limits
  tool_parse_buffer_max: 1048576 # 1MB max per tool definition
  text_buffer_max: 10485760 # 10MB max text accumulation

  # Supported formats
  formats:
    xml:
      tag_open: "<tool_use>"
      tag_close: "</tool_use>"
      attributes: [id, name]

    json:
      schema:
        type: object
        required: [type, name, input]
        properties:
          type: { const: "tool_use" }
          id: { type: string }
          name: { type: string }
          input: { type: object }
```

## Transcript Events

With streaming, events interleave in real-time execution order:

```
Timeline:
├─ LLM starts generating
├─ cognition_out_delta (chunk 1)
├─ cognition_out_delta (chunk 2)
├─ tool definition completes
│  ├─ tool_call_start (immediately)
│  ├─ tool executes...
│  └─ tool_call_result (when done, may be mid-stream)
├─ cognition_out_delta (chunk 3, while tool runs)
├─ cognition_out_delta (chunk 4)
└─ LLM finishes
   └─ cognition_out (complete text)
```

**Event Ordering Guarantees:**

- `tool_call_start` emitted when tool definition completes (mid-stream)
- `tool_call_result` emitted when tool completes (may be before stream ends)
- `cognition_out` always emitted after stream completes (contains full text)
- `cognition_out_delta` events may be interleaved with tool events

## Replay from Interleaved Transcript

Reconstruction must handle interleaved events:

```python
def reconstruct_from_transcript(events: List[Dict]) -> List[Dict]:
    """Rebuild conversation from interleaved transcript events."""
    messages = []
    text_buffer = []
    pending_tools = {}

    for event in events:
        event_type = event.get("type")

        if event_type == "cognition_out_delta":
            # Accumulate text deltas
            text_buffer.append(event["text"])

        elif event_type == "cognition_out":
            # Final text - use this, not accumulated deltas
            messages.append({
                "role": "assistant",
                "content": event["text"],
            })
            text_buffer.clear()

        elif event_type == "tool_call_start":
            # Track pending tool
            pending_tools[event["call_id"]] = {
                "type": "tool_use",
                "id": event["call_id"],
                "name": event["tool"],
                "input": event["input"],
            }

        elif event_type == "tool_call_result":
            # Match result to call by ID
            call_id = event["call_id"]
            if call_id in pending_tools:
                tool_call = pending_tools.pop(call_id)
                tool_call["output"] = event.get("output")
                tool_call["error"] = event.get("error")
                messages.append(tool_call)

    return messages
```

## Error Handling

**Stream Failure Mid-Response:**

- Completed tools: results preserved in transcript
- Incomplete tools: definitions discarded (never executed)
- Retry reconstructs context including completed tool results

**Tool Parse Error:**

- Malformed tool definition → error event, continue parsing
- Buffer overflow → error, clear buffer, continue

## Benefits

1. **Lower Latency** - Tools start executing ~50-200ms earlier
2. **Better UX** - Real-time transcript tailing shows progress
3. **True Parallelism** - LLM generates while tools execute

## Tradeoffs

1. **Complexity** - Partial tool accumulation, interleaved events
2. **Replay** - More complex reconstruction logic
3. **Debugging** - Interleaved transcript harder to read linearly

## Testing

```python
class TestStreamingToolParser:
    test_xml_format_single_tool
    test_xml_format_multiple_tools
    test_partial_accumulation
    test_text_between_tools
    test_json_format_single_tool
    test_mixed_text_and_tools
    test_malformed_tool_handling
    test_buffer_overflow_protection

class TestStreamingToolExecution:
    test_tool_executes_during_stream
    test_tool_result_before_stream_ends
    test_multiple_tools_batched
    test_batch_size_threshold
    test_batch_delay_timer
    test_groups_by_item_id

class TestStreamingTranscript:
    test_interleaved_events_order
    test_cognition_out_always_last
    test_reconstruct_from_interleaved
    test_accumulate_deltas_to_full_text
```

## Error Handling & Partial Cognition

### Stream Interruption Recovery

When streaming is interrupted, partial data is preserved:

```python
async def _run_tool_use_loop_streaming(...):
    parser = StreamingToolParser()
    completed_tools = []

    try:
        async for chunk in _stream_llm_with_retry(...):
            # ... process chunks ...
            pass

        # Success - emit complete cognition
        transcript.write_event(thread_id, "cognition_out", {
            "text": parser.get_full_text(),
            "is_partial": False,
        })

    except Exception as e:
        # FAILURE - emit partial cognition for resume
        partial_text = parser.get_full_text()

        transcript.write_event(thread_id, "cognition_out", {
            "text": partial_text,
            "is_partial": True,
            "truncated": True,
            "error": str(e),
            "completion_percentage": estimate_completion(partial_text),
        })

        # Return with partial results preserved
        return {
            "success": False,
            "error": str(e),
            "partial_text": partial_text,
            "completed_tools": completed_tools,  # Tools that finished before error
        }
```

### Preserved State on Error

When an error occurs mid-stream:

1. **`cognition_out` with `is_partial: true`** - Accumulated text preserved
2. **`tool_call_result` events** - Tools that completed before error
3. **Deltas may be incomplete** - Droppable, not required for replay

### Resume from Partial State

```python
async def resume_thread(thread_id: str, ...):
    # Load events from transcript
    events = read_transcript(thread_id)

    # Find last cognition (may be partial)
    last_cognition = find_last_cognition_out(events)

    # Build context including partial cognition
    context = {
        "messages": reconstruct_messages(events),
        "partial_cognition": last_cognition if last_cognition.get("is_partial") else None,
    }

    # Continue LLM from partial state
    if context["partial_cognition"]:
        # Add context note about interruption
        context["messages"].append({
            "role": "system",
            "content": f"[Previous response was interrupted at {last_cognition.get('completion_percentage', '?')}%]"
        })

    return await continue_thread(thread_id, context)
```

**User sees:**

```
[Previous response was interrupted at 65%]

Let me continue from where I left off...
```

### Partial Reasoning on Error

Reasoning blocks (thinking/CoT) are accumulated during streaming and emitted on error:

```python
async def _run_tool_use_loop_streaming(...):
    parser = StreamingToolParser()
    reasoning_buffer = []

    try:
        async for chunk in _stream_llm_with_retry(...):
            # Handle reasoning blocks (if provider supports them)
            if chunk.type == "reasoning":
                reasoning_buffer.append(chunk.content)
                emit_droppable(transcript, thread_id, "cognition_reasoning", {
                    "text": chunk.content,
                })

            # ... handle text and tools ...

    except Exception as e:
        # Emit partial reasoning if any was accumulated
        if reasoning_buffer:
            transcript.write_event(thread_id, "cognition_reasoning", {
                "text": "".join(reasoning_buffer),
                "is_partial": True,
                "was_interrupted": True,
            })

        # ... emit partial cognition_out ...
```

**Transcript on error shows:**

```
cognition_reasoning: "Let me analyze this step by step. First..." [is_partial: true]
cognition_out: "The solution involves..." [is_partial: true]
```
