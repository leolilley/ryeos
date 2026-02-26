
```yaml
name: streaming
title: Per-Token Streaming
entry_type: reference
category: rye/agent/threads
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - streaming
  - sse
  - transcripts
  - real-time
references:
  - persistence-and-state
  - thread-lifecycle
  - "docs/orchestration/streaming.md"
```

# Per-Token Streaming

Real-time token streaming from LLM providers to `transcript.jsonl` (as `token_delta` JSONL events) and knowledge markdown (as appended text). Enables live `tail -f` monitoring of thread execution.

## Sink Architecture

`TranscriptSink` implements `write(event)` and `close()` for the `HttpClientPrimitive` fan-out interface. During streaming, the primitive dispatches each parsed SSE event to all registered sinks:

```python
class TranscriptSink:
    def write(self, event: dict) -> None:
        # 1. Append token_delta JSONL line to transcript.jsonl
        # 2. Append text to knowledge markdown
        ...

    def close(self) -> None:
        # Flush and finalize
        ...
```

The fan-out sends to both `TranscriptSink` (persistence) and `ReturnSink` (in-memory response assembly).

## Key Files

| File | Role |
|------|------|
| `transcript_sink.py` | `TranscriptSink` — writes `token_delta` events to JSONL, appends text to knowledge markdown |
| `http_provider.py` | `create_streaming_completion()`, `_assemble_anthropic_stream()`, `_assemble_openai_stream()` |
| `runner.py` | Streaming path — selects `create_streaming_completion` when `supports_streaming` is true |
| `provider_adapter.py` | `supports_streaming` property — determines whether the provider can stream |

## The `__dunder` Key Passthrough

`primitive_executor.py` uses `__dunder` prefixed keys for non-serializable parameters (like sink objects) that must pass through to the HTTP primitive without being included in the serialized request body:

```python
# In http_provider.py
execution_config["__sinks"] = [transcript_sink, return_sink]

# In primitive_executor.py — strips __dunder keys before serialization
sinks = config.pop("__sinks", [])
```

This prevents non-serializable objects from hitting JSON encoding while keeping the execution config as the single parameter carrier.

## Anthropic SSE Event Flow

| Event Type | Data | Used For |
|------------|------|----------|
| `message_start` | `message.usage.input_tokens` | Input token count |
| `content_block_start` | Block type and index | Initialize accumulation buffer |
| `content_block_delta` | `delta.text` or `delta.partial_json` | Stream text tokens / tool input fragments |
| `message_delta` | `usage.output_tokens`, `stop_reason` | Output token count, stop reason |
| `message_stop` | — | End of stream |

Text tokens arrive via `content_block_delta` with `delta.type == "text_delta"`. Tool input arrives as `delta.type == "input_json_delta"` with `delta.partial_json` fragments.

## Response Assembly

After the SSE stream closes, accumulated parts are assembled into the same response dict as non-streaming:

```python
response = {
    "content": [
        {"type": "text", "text": "".join(text_parts)},
        # tool_use blocks with input reassembled from input_parts
    ],
    "usage": {
        "input_tokens": message_start_usage["input_tokens"],
        "output_tokens": message_delta_usage["output_tokens"],
    },
    "stop_reason": stop_reason,  # from message_delta
}
```

- `text_parts`: list of strings from `content_block_delta` text events, joined at assembly
- `tool_calls`: each tool use block accumulates `input_parts` (partial JSON strings) which are concatenated and parsed at assembly
- `usage`: input tokens from `message_start`, output tokens from `message_delta`

## The `stream` Body Parameter

The `stream: true` parameter is declared in `anthropic.yaml` (and equivalent provider configs) as part of the request body template. When streaming is not active, the parameter is auto-stripped by the unresolved-placeholder stripping logic in `_build_execution_config()` — any body parameter whose value is an unresolved placeholder (e.g., `{{stream}}`) is removed before the request is sent.

```yaml
# In anthropic.yaml
body:
  stream: "{{stream}}"  # Stripped when not provided, set to true for streaming
```

## Integration with render_knowledge

`render_knowledge()` rewrites the knowledge markdown cleanly at each checkpoint. Between checkpoints, streaming deltas accumulate at the end of the file as raw appended text. At the next checkpoint, the full file is regenerated from structured data, incorporating the streamed content.

## Graph Observability

Graphs use the same JSONL transcript pattern as threads but emit discrete events instead of token deltas. There is no SSE stream and no `TranscriptSink` — the graph walker (`walker.py`) writes events directly to `transcript.jsonl`.

Graph event types: `graph_started`, `step_started`, `step_completed`, `foreach_completed`, `graph_completed`, `graph_error`, `graph_cancelled`.

Events are checkpoint-signed at step boundaries using the same `TranscriptSigner`. The knowledge markdown is fully re-rendered from JSONL at each step (not incrementally appended like thread streaming), producing a visual node status table with completion indicators (✅/🔄/⏳/❌).

See `persistence-and-state` for the full storage layout.
