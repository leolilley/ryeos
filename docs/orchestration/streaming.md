```yaml
id: streaming
title: "Per-Token Streaming"
description: Real-time token streaming to transcript JSONL and knowledge markdown
category: orchestration
tags: [streaming, transcripts, sse, real-time]
version: "1.0.0"
```

# Per-Token Streaming

Threads now stream LLM tokens in real-time to both `transcript.jsonl` and knowledge markdown. Instead of waiting for the full response before writing, each token is emitted as it arrives — giving live visibility into what the model is generating.

## Architecture

```
HttpProvider
  └── dispatcher (create_streaming_completion)
        └── HttpClientPrimitive (SSE mode)
              └── sink fan-out
                    ├── TranscriptSink  → transcript.jsonl + knowledge.md
                    └── ReturnSink     → in-memory response assembly
```

`HttpProvider` checks `supports_streaming` on the provider adapter. If true, it calls `create_streaming_completion()` instead of `create_completion()`. The `HttpClientPrimitive` opens an SSE connection and fans out parsed events to registered sinks.

## TranscriptSink

`TranscriptSink` implements `write(event)` and `close()` for the `HttpClientPrimitive` fan-out interface. On each content delta:

1. **JSONL** — Appends a `token_delta` event to `transcript.jsonl`
2. **Knowledge markdown** — Appends the raw text to the knowledge markdown file at `.ai/knowledge/threads/{thread_id}.md`

This means both files update in real-time as the model generates tokens.

## Watching Live Output

### JSONL transcript

```bash
tail -f .ai/agent/threads/<thread_id>/transcript.jsonl
```

### Knowledge markdown

```bash
tail -F .ai/knowledge/threads/<thread_id>.md
```

Use `-F` (capital) for knowledge markdown — `render_knowledge()` rewrites the file at checkpoints, which replaces the inode.

### Pretty-printed text deltas

```bash
tail -f .ai/agent/threads/<thread_id>/transcript.jsonl \
  | python3 -c "import sys,json;[print(json.loads(l).get('payload',{}).get('text',''),end='',flush=True) for l in sys.stdin]"
```

## Async Execution

Use `async_exec: true` to fire a thread in the background, then tail its output:

```python
result = rye_execute(
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "execute_directive",
        "directive_name": "my-directive",
        "async_exec": True,
    },
)
thread_id = result["thread_id"]
# Now tail -f .ai/agent/threads/{thread_id}/transcript.jsonl
```

## Event Format

Each streaming token produces a `token_delta` event in the JSONL transcript:

```json
{"timestamp": "2026-02-18T10:00:01.234Z", "thread_id": "my-directive-1739820456", "event_type": "token_delta", "payload": {"text": "Hello", "index": 0}}
```

| Field | Type | Description |
|-------|------|-------------|
| `timestamp` | string | ISO 8601 timestamp |
| `thread_id` | string | Thread producing the event |
| `event_type` | string | Always `token_delta` for streaming deltas |
| `payload.text` | string | The token text fragment |
| `payload.index` | int | Content block index (for multi-block responses) |

## SSE Format Handling

The `HttpClientPrimitive` handles two SSE wire formats:

| Provider | SSE Format | Content Delta Event |
|----------|-----------|---------------------|
| Anthropic | Event-type SSE (`event:` + `data:` lines) | `content_block_delta` with `delta.text` |
| OpenAI | Data-only SSE (`data:` lines only) | `choices[0].delta.content` |

### Anthropic SSE Event Flow

```
event: message_start       → usage.input_tokens
event: content_block_start → content block metadata
event: content_block_delta → delta.text (streamed tokens)
event: message_delta       → usage.output_tokens, stop_reason
event: message_stop        → end of stream
```

### OpenAI SSE Event Flow

```
data: {"choices": [{"delta": {"content": "token"}}]}
data: {"choices": [{"delta": {"content": "text"}}]}
data: [DONE]
```

## Response Assembly

Buffered SSE events are reassembled into the same response dict structure as non-streaming calls. The `ReturnSink` accumulates:

| Accumulated Field | Source |
|-------------------|--------|
| `text_parts` | Concatenated `content_block_delta` text fragments |
| `tool_calls` | Tool use blocks with `input_parts` JSON accumulation |
| `usage` | Merged from `message_start` (input) + `message_delta` (output) |
| `stop_reason` | From `message_delta` |

After the stream closes, the assembled response is returned to the runner as if it came from a non-streaming `create_completion()` call. The runner's turn loop is unaware of the streaming internals.

## Fallback

Providers that don't support streaming (`supports_streaming = False` on the provider adapter) use `create_completion()` as before. No sinks are created, no SSE connection is opened. The response is returned as a single dict.

```python
if provider_adapter.supports_streaming:
    response = provider.create_streaming_completion(messages, tools, sinks=[transcript_sink])
else:
    response = provider.create_completion(messages, tools)
```

## Integration with render_knowledge

`render_knowledge()` rewrites the knowledge markdown file cleanly at each checkpoint — rebuilding the full cognition-framed transcript from structured data.

Between checkpoints, streaming deltas accumulate at the end of the file as raw appended text. This means:

1. During streaming: the file grows incrementally with each token
2. At checkpoint: `render_knowledge()` rewrites the entire file, incorporating the streamed content into the structured format
3. After checkpoint: new streaming deltas again accumulate at the end

This is why `tail -F` (follow by name) is required — the file is replaced at each checkpoint.

## What's Next

- [Thread Lifecycle](./thread-lifecycle.md) — Full execution flow including the streaming path
- [Continuation and Resumption](./continuation-and-resumption.md) — How threads persist and resume
