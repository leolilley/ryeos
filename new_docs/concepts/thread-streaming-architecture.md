# Thread Streaming Architecture

> Complete picture of how LLM streaming, HTTP primitive, sinks, and tool execution work together.
>
> **See also:**
> - [thread-streaming-execution.md](thread-streaming-execution.md) - Tool execution details
> - [data-driven-thread-events.md](data-driven-thread-events.md) - Event types and schemas
> - [thread-orchestration-internals.md](thread-orchestration-internals.md) - Implementation gaps

## The Complete Flow

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                         End-to-End Streaming Flow                            │
├──────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  ┌─────────────────┐                                                        │
│  │  Anthropic API  │                                                        │
│  │  POST /v1/messages?stream=true                                           │
│  │  SSE: data: {"type": "content_block_delta"...}                            │
│  └────────┬────────┘                                                        │
│           │ HTTP/SSE Stream                                                  │
│           ▼                                                                  │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                    HTTP Primitive (lilux)                              │  │
│  │  mode: "stream"  ──┬──► WebSocketSink ──► UI (real-time)             │  │
│  │                    ├──► FileSink ──► audit log                        │  │
│  │                    └──► ReturnSink ──► Thread Directive               │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│           │                                                                  │
│           ▼                                                                  │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                    Thread Directive                                    │  │
│  │                                                                        │  │
│  │  ┌─► StreamingToolParser                                              │  │
│  │  │    ├─► Tool call complete ──► Execute immediately                  │  │
│  │  │    └─► Batched dispatch (100ms or 5 tools)                         │  │
│  │  │                                                                     │  │
│  │  ├─► cognition_out_delta ──► emit_droppable() ──► Transcript         │  │
│  │  │                                                                     │  │
│  │  ├─► cognition_reasoning ──► emit_droppable() ──► Transcript         │  │
│  │  │                                                                     │  │
│  │  └─► Stream ends ──► cognition_out (critical) ──► Transcript         │  │
│  │       (complete text, always emitted even on error)                   │  │
│  │                                                                        │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│           │                                                                  │
│           ▼                                                                  │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                    Transcript JSONL                                    │  │
│  │                                                                        │  │
│  │  {"type": "cognition_out_delta", "text": "Let me..."}     ← droppable │  │
│  │  {"type": "cognition_reasoning", "text": "First I need..."} ← droppable│  │
│  │  {"type": "tool_call_start", "tool": "read_file"}         ← critical  │  │
│  │  {"type": "cognition_out_delta", "text": " analyze the..."} ← droppable│  │
│  │  {"type": "tool_call_result", "output": "..."}            ← critical  │  │
│  │  {"type": "cognition_out", "text": "Let me analyze..."}   ← critical  │  │
│  │       ↑ complete text (includes all deltas)                            │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
```

## Key Components

### 1. HTTP Primitive (`lilux/primitives/http_client.py`)

Already supports streaming SSE with sink fan-out:

```python
# Provider YAML configures streaming
provider:
  name: "anthropic"
  
stream:
  enabled: true
  sinks:
    - type: websocket
      url: "ws://localhost:8080/events"
    - type: file
      path: ".ai/threads/{thread_id}/stream.jsonl"

# HTTP primitive handles the SSE parsing and fan-out
async with client.stream(...) as response:
    async for line in response.aiter_lines():
        if line.startswith("data:"):
            event_data = line[5:].strip()
            for sink in sinks:
                await sink.write(event_data)  # Fan-out
```

### 2. Sinks (fan-out destinations)

| Sink Type | Purpose | Critical? | Use Case |
|-----------|---------|-----------|----------|
| `WebSocketSink` | UI real-time updates | Droppable | VS Code extension, web UI |
| `FileSink` | Audit trail | Droppable | Post-mortem debugging |
| `ReturnSink` | Thread directive consumption | **Required** | Parse chunks, emit events |

### 3. StreamingToolParser

Consumes ReturnSink events, extracts tool calls:

```python
parser = StreamingToolParser()

async for chunk in return_sink:
    for event_type, data in parser.feed_chunk(chunk):
        if event_type == 'tool_complete':
            pending_tools.append(data)
            if len(pending_tools) >= 5:
                await execute_batch(pending_tools)
```

### 4. Thread Events

Two event types with different guarantees:

**Droppable (UI only):**
- `cognition_out_delta` - Text chunk
- `cognition_reasoning` - Thinking block
- Can be lost without affecting correctness

**Critical (required for replay):**
- `cognition_out` - Complete text (or partial on error)
- `tool_call_start` - Tool execution begins
- `tool_call_result` - Tool execution completes

### 5. Partial Cognition on Error

```python
try:
    async for chunk in stream:
        full_text += chunk
    
    transcript.write_event("cognition_out", {
        "text": full_text,
        "is_partial": False
    })
    
except Exception as e:
    # Stream failed - emit partial
    transcript.write_event("cognition_out", {
        "text": full_text,  # Accumulated so far
        "is_partial": True,
        "error": str(e),
        "completion_percentage": estimate_completion(full_text)
    })
```

## Data Flow Summary

```
Anthropic SSE ──► HTTP Primitive ──┬──► WebSocketSink ──► UI
                                   ├──► FileSink ──► Log
                                   └──► ReturnSink ──► Thread Directive
                                                         │
                                                         ├─► StreamingToolParser
                                                         │   └───► Execute tools
                                                         │
                                                         ├─► cognition_out_delta
                                                         │   └───► Transcript (droppable)
                                                         │
                                                         └─► cognition_out
                                                             └───► Transcript (critical)
```

## Configuration

### Provider Config

```yaml
# rye/rye/.ai/tools/rye/agent/threads/config/providers/anthropic.yaml
provider:
  name: anthropic
  api_url: https://api.anthropic.com/v1/messages
  
  auth:
    type: bearer
    token: ${ANTHROPIC_API_KEY}

stream:
  enabled: true
  format: sse
  
  # Fan-out to multiple sinks
  sinks:
    # UI real-time (optional, droppable)
    - type: websocket
      url: ${THREAD_UI_WEBSOCKET_URL:-ws://localhost:8080/events}
      reconnect_attempts: 3
      buffer_on_disconnect: true
      buffer_max_size: 1000
    
    # Audit log (optional, droppable)  
    - type: file
      path: .ai/threads/{thread_id}/sse-audit.jsonl
      format: jsonl
      rotate: true
      max_size: 10MB
  
  # What to extract from SSE (JSONPath)
  extract:
    text: "$.delta.text"
    tool_calls: "$.content_blocks[?(@.type=='tool_use')]"
    reasoning: "$.content_blocks[?(@.type=='thinking')]"
    stop_reason: "$.stop_reason"
```

### Thread Directive Config

```yaml
# rye/rye/.ai/tools/rye/agent/threads/config/streaming.yaml
streaming:
  # Tool batching
  tool_batch_size: 5      # Execute immediately if N tools ready
  tool_batch_delay: 0.1   # Wait N seconds for more tools
  
  # Parser limits
  tool_parse_buffer_max: 1048576   # 1MB per tool def
  text_buffer_max: 10485760        # 10MB total text
  
  # Error handling
  emit_partial_on_error: true      # Always emit cognition_out on error
  preserve_completed_tools: true   # Keep tool results on error
```

## Testing Strategy

```python
class TestStreamingIntegration:
    test_http_primitive_sse_parsing
    test_sink_fan_out_webbsocket
    test_sink_fan_out_file
    test_return_sink_to_directive
    test_tool_parser_from_sse
    test_cognition_delta_emission
    test_partial_cognition_on_error
    test_streaming_end_to_end
```

## Benefits

1. **Low Latency** - Tools execute during LLM generation
2. **Real-time UI** - WebSocket delivers deltas immediately
3. **Audit Trail** - File sink captures complete stream
4. **Resilient** - Partial cognition preserved on error
5. **Composable** - Sinks can be added/removed per provider
