# HttpClientPrimitive

## Purpose

Make HTTP requests with retry logic, authentication, and SSE streaming support.

## Key Classes

### HttpResult

```python
@dataclass
class HttpResult:
    success: bool                      # Whether request succeeded (2xx/3xx)
    status_code: int                   # HTTP status code
    body: Any                          # Response body (JSON, text, or List[str] for streaming)
    headers: Dict[str, str]            # Response headers
    duration_ms: int                   # Total request time in milliseconds
    error: Optional[str] = None        # Error message if failed
    stream_events_count: Optional[int] = None  # For streaming mode
    stream_destinations: Optional[List[str]] = None  # Sink class names used
```

### StreamDestination & StreamConfig

```python
@dataclass
class StreamDestination:
    type: str                          # "return" (built-in) or tool-based sinks
    path: Optional[str] = None         # For file sinks
    config: Optional[Dict[str, Any]] = None
    format: str = "jsonl"

@dataclass
class StreamConfig:
    transport: str                     # "sse" only (WebSocket is a separate tool sink)
    destinations: List[StreamDestination]
    buffer_events: bool = False
    max_buffer_size: int = 10_000
```

### ReturnSink

Built-in sink for buffering streaming events:

```python
class ReturnSink:
    def __init__(self, max_size: int = 10000): ...
    async def write(self, event: str) -> None: ...
    async def close(self) -> None: ...
    def get_events(self) -> List[str]: ...
```

## The `params` Parameter

| `params` Key | Purpose | Example |
|-------------|---------|---------|
| `mode` | Select sync or stream mode | `{"mode": "stream"}` |
| `{any_key}` | URL templating via `url.format(**params)` | `{"user_id": "123"}` |
| `{any_key}` | Body templating (type-preserving for single placeholders) | `{"messages": [...]}` |
| `__sinks` | Pre-instantiated sink objects (set by orchestrator) | `[ReturnSink()]` |

## Configuration

### Required

- **`url`** (str): Target URL with optional `{param}` templating and `${VAR:-default}` env vars

### Optional

- **`method`** (str): HTTP method (default: `"GET"`)
- **`headers`** (dict): Request headers (supports `${VAR:-default}` env vars)
- **`body`** (any): Request body with recursive templating
- **`timeout`** (int): Request timeout in seconds (default: 30)
- **`retry`** (dict): Retry configuration
  - `max_attempts` (int): Max retries (default: 1)
  - `backoff` (str): `"exponential"` or `"linear"`
- **`auth`** (dict): Authentication configuration
  - `type` (str): `"bearer"` or `"api_key"`
  - `token` (str): For bearer auth (supports `${VAR}`)
  - `key` (str): For api_key auth (supports `${VAR}`)
  - `header` (str): Custom header name for api_key (default: `"X-API-Key"`)

## Mode Switching

Use `params["mode"]` to select execution mode:

```python
# Sync mode (default)
result = await client.execute(config, {"mode": "sync"})

# Stream mode (SSE)
result = await client.execute(config, {"mode": "stream", "__sinks": [ReturnSink()]})
```

## Authentication

### Bearer Token

```python
config = {
    "url": "https://api.example.com/data",
    "auth": {
        "type": "bearer",
        "token": "${API_TOKEN}"  # Resolved from environment
    }
}
# Results in: Authorization: Bearer <token_value>
```

### API Key

```python
config = {
    "url": "https://api.example.com/data",
    "auth": {
        "type": "api_key",
        "key": "${API_KEY}",
        "header": "X-Custom-Key"  # Optional, defaults to X-API-Key
    }
}
# Results in: X-Custom-Key: <key_value>
```

## Environment Variable Resolution

URLs, headers, and auth credentials support `${VAR:-default}` syntax:

```python
config = {
    "url": "${API_BASE_URL:-https://api.example.com}/users",
    "headers": {
        "X-Tenant": "${TENANT_ID}"
    },
    "auth": {
        "type": "bearer",
        "token": "${AUTH_TOKEN}"
    }
}
```

## Body Templating

Recursive templating with type preservation for single placeholders:

```python
config = {
    "url": "https://api.openai.com/v1/chat/completions",
    "method": "POST",
    "body": {
        "model": "{model}",           # String replacement
        "messages": "{messages}",      # Preserves list type
        "temperature": "{temperature}" # Preserves float type
    }
}

params = {
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello"}],  # List preserved
    "temperature": 0.7  # Float preserved
}
```

## Streaming (SSE Only)

SSE streaming with sink fan-out:

```python
from lilux.primitives.http_client import HttpClientPrimitive, ReturnSink

client = HttpClientPrimitive()
sink = ReturnSink()

result = await client.execute(
    config={
        "url": "https://api.example.com/stream",
        "method": "POST",
        "body": {"prompt": "{prompt}"}
    },
    params={
        "mode": "stream",
        "prompt": "Hello world",
        "__sinks": [sink]
    }
)

events = result.body  # List[str] of SSE event data
print(f"Received {result.stream_events_count} events")
```

**Note:** WebSocket streaming is handled by a separate data-driven tool sink (`.ai/tools/sinks/websocket_sink.py`), not built into HttpClientPrimitive.

## Sink Architecture

Sinks are pre-instantiated by the orchestrator and passed via `params["__sinks"]`:

```
┌─────────────────────────────────────────┐
│  Orchestrator (RYE)                     │
│  1. Parse stream destinations           │
│  2. Instantiate sink objects            │
│  3. Pass via params["__sinks"]          │
└──────────────┬──────────────────────────┘
               │
               ▼
┌─────────────────────────────────────────┐
│  HttpClientPrimitive (Lilux)            │
│  1. Extract sinks: params.pop("__sinks")│
│  2. Stream SSE events                   │
│  3. Fan-out: await sink.write(event)    │
│  4. Cleanup: await sink.close()         │
└─────────────────────────────────────────┘
```

### Sink Interface

All sinks must implement:

```python
class SinkProtocol:
    async def write(self, event: str) -> None: ...
    async def close(self) -> None: ...
```

### Available Sinks

- **ReturnSink** (built-in): Buffers events in memory, returned in `result.body`
- **Tool-based sinks** (external): `file_sink`, `null_sink`, `websocket_sink` - implemented as data-driven tools

## Retry Strategy

Retries on network errors and transient failures with exponential backoff:

```python
config = {
    "url": "https://api.example.com/data",
    "retry": {
        "max_attempts": 3,
        "backoff": "exponential"  # 1s, 2s, 4s delays
    }
}
```

Retries on:
- Network errors (connection refused, timeout, DNS)
- httpx exceptions (TimeoutException, ConnectError, RequestError)

## Error Handling

All errors returned as `HttpResult`, never thrown:

```python
result = await client.execute(config, params)

if not result.success:
    print(f"Error: {result.error}")
    print(f"Status: {result.status_code}")
```

## Connection Pooling

HttpClientPrimitive maintains a connection pool:

```python
client = HttpClientPrimitive()

# Reuses connections across requests
await client.execute(config1, params1)
await client.execute(config2, params2)

# Clean up when done
await client.close()
```

## Complete Example

```python
from lilux.primitives.http_client import HttpClientPrimitive, ReturnSink

async def call_openai():
    client = HttpClientPrimitive()
    
    try:
        result = await client.execute(
            config={
                "url": "https://api.openai.com/v1/chat/completions",
                "method": "POST",
                "headers": {"Content-Type": "application/json"},
                "auth": {
                    "type": "bearer",
                    "token": "${OPENAI_API_KEY}"
                },
                "body": {
                    "model": "{model}",
                    "messages": "{messages}",
                    "stream": False
                },
                "timeout": 60,
                "retry": {"max_attempts": 2}
            },
            params={
                "mode": "sync",
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello!"}]
            }
        )
        
        if result.success:
            return result.body["choices"][0]["message"]["content"]
        else:
            raise Exception(result.error)
    finally:
        await client.close()
```

## Next Steps

- See subprocess: [[lilux/primitives/subprocess]]
- See lockfile: [[lilux/primitives/lockfile]]
- See runtime services: [[lilux/runtime-services/overview]]
