# Data-Driven Streaming Configuration

> Configuration for HTTP streaming, SSE parsing, sinks, and event extraction
>
> **Location:** `rye/rye/.ai/tools/rye/agent/threads/config/streaming.yaml`

## Overview

All streaming behavior is data-driven from YAML:
- HTTP primitive streaming mode
- Sink configurations (WebSocket, File, etc.)
- SSE event parsing and extraction
- Event emission rules (droppable vs critical)

## Configuration Schema

```yaml
# streaming.yaml
schema_version: "1.0.0"

streaming:
  # HTTP Primitive Streaming
  http:
    enabled: true
    mode: "stream"                    # "stream" | "sync"
    transport: "sse"                  # Server-Sent Events
    
    # Connection settings
    connection:
      timeout: 120                    # seconds for entire stream
      read_timeout: 60                # seconds between chunks
      retry_on_disconnect: true
      max_reconnects: 3
    
    # SSE parsing
    sse:
      event_prefix: "data:"
      event_types: ["message", "content_block_delta", "content_block_stop"]
      ignore_empty_lines: true
      ignore_comments: true           # Lines starting with :

  # Sink Configuration (fan-out destinations)
  sinks:
    enabled: true
    
    # Global sink settings
    global:
      max_concurrent_writes: 100
      write_timeout_seconds: 5
      buffer_on_backpressure: true
      buffer_max_size: 10000
    
    # Sink definitions
    definitions:
      websocket_ui:
        type: "websocket"
        enabled: true
        url: "${THREAD_UI_WEBSOCKET_URL:-ws://localhost:8080/events}"
        reconnect:
          attempts: 3
          backoff: "exponential"       # exponential | fixed | linear
          base_delay: 0.5              # seconds
          max_delay: 30.0
        buffer:
          on_disconnect: true
          max_size: 1000
          drop_policy: "oldest"        # oldest | newest
        compression:
          enabled: false               # gzip | deflate | none
          
      file_audit:
        type: "file"
        enabled: true
        path: ".ai/threads/{thread_id}/sse-raw.jsonl"
        format: "jsonl"
        rotation:
          enabled: true
          max_size_mb: 10
          max_files: 5
          compress_rotated: true
        
      return_sink:
        type: "return"
        enabled: true                  # Always enabled for directive
        buffer_size: 10000             # Max events to buffer

  # Event Extraction (from SSE to thread events)
  extraction:
    enabled: true
    
    # Extraction rules using JSONPath
    rules:
      text_delta:
        enabled: true
        match:
          path: "$.type"
          value: "content_block_delta"
        extract:
          text: "$.delta.text"
          index: "$.index"
        emit:
          event: "cognition_out_delta"
          criticality: "droppable"
          throttle: "1s"                # Max 1 event per second
          
      reasoning:
        enabled: true
        match:
          path: "$.type"
          value: "thinking"
        extract:
          text: "$.thinking"
        emit:
          event: "cognition_reasoning"
          criticality: "droppable"
          accumulate_on_error: true
          
      tool_use_start:
        enabled: true
        match:
          path: "$.type"
          value: "tool_use"
        extract:
          tool_id: "$.id"
          tool_name: "$.name"
          tool_input: "$.input"
        emit:
          event: "cognition_in"        # Tool call as input to next turn
          criticality: "critical"
          
      tool_use_delta:
        enabled: true
        match:
          path: "$.type"
          value: "tool_use_delta"
        extract:
          partial_input: "$.partial_json"
        accumulate:
          by: "$.id"                    # Accumulate by tool_id
          until: "tool_use_stop"
          
      completion:
        enabled: true
        match:
          path: "$.type"
          value: "message_stop"
        extract:
          stop_reason: "$.stop_reason"
          usage: "$.usage"
        emit:
          event: "step_finish"
          criticality: "critical"

  # Event Emission Configuration
  emission:
    # Droppable events (fire-and-forget)
    droppable:
      cognition_out_delta:
        enabled: true
        async: true
        throttle: "1s"
        max_queue_size: 1000
        drop_policy: "oldest"
        
      cognition_reasoning:
        enabled: true
        async: true
        accumulate: true              # Buffer, emit on error
        
      tool_call_progress:
        enabled: true
        async: true
        throttle: "1s"
        milestones: [0, 25, 50, 75, 100]
    
    # Critical events (blocking, guaranteed)
    critical:
      cognition_out:
        enabled: true
        async: false
        emit_on_error: true           # Always emit partial
        required_fields: [text, is_partial]
        
      tool_call_start:
        enabled: true
        async: false
        
      tool_call_result:
        enabled: true
        async: false
        
      step_start:
        enabled: true
        async: false
        
      step_finish:
        enabled: true
        async: false

  # Tool Parsing (StreamingToolParser)
  tool_parsing:
    enabled: true
    
    # Supported formats
    formats:
      xml:
        enabled: true
        tag_open: "<tool_use>"
        tag_close: "</tool_use>"
        attributes:
          - id
          - name
        parse_strategy: "accumulate_until_close"
        
      json:
        enabled: true
        schema:
          type: object
          required: [type, name]
          properties:
            type: {const: "tool_use"}
            id: {type: string}
            name: {type: string}
            input: {type: object}
        parse_strategy: "incremental_json"
    
    # Parser limits
    limits:
      max_tool_size: 1048576          # 1MB per tool definition
      max_concurrent_tools: 50
      max_text_buffer: 10485760       # 10MB total text
    
    # Batching
    batch:
      size_threshold: 5               # Execute when N tools ready
      delay_seconds: 0.1              # Or after N seconds
      max_wait_for_first: 5.0

  # Error Handling
  error_handling:
    # On stream interruption
    on_interruption:
      emit_partial_cognition: true
      emit_partial_reasoning: true
      preserve_completed_tools: true
      
    # Partial cognition schema
    partial_cognition:
      required_fields: [text, is_partial]
      optional_fields: [error, truncated, completion_percentage]
      estimate_completion: true       # Try to estimate % complete

# Provider-Specific Configuration
providers:
  anthropic:
    extends: "streaming"
    
    http:
      url: "https://api.anthropic.com/v1/messages"
      headers:
        anthropic-version: "2023-06-01"
        content-type: "application/json"
    
    extraction:
      rules:
        text_delta:
          extract:
            text: "$.delta.text"
            
        reasoning:
          match:
            path: "$.type"
            value: "thinking"
            
  openai:
    extends: "streaming"
    
    http:
      url: "https://api.openai.com/v1/chat/completions"
    
    extraction:
      rules:
        text_delta:
          extract:
            text: "$.choices[0].delta.content"
            
        tool_use:
          match:
            path: "$.choices[0].delta.tool_calls"
            exists: true
```

## Usage Examples

```python
# Load streaming config
config = load_config("streaming.yaml", project_path)

# Build sinks from config
sinks = []
for sink_name, sink_config in config.sinks.definitions.items():
    if not sink_config.enabled:
        continue
        
    if sink_config.type == "websocket":
        sinks.append(WebSocketSink(
            url=sink_config.url,
            reconnect_attempts=sink_config.reconnect.attempts,
            ...
        ))

# Call HTTP primitive with sinks
result = await http_primitive.execute(
    config={
        "url": config.providers.anthropic.http.url,
        "method": "POST",
        "stream": True,
    },
    params={
        "mode": "stream",
        "__sinks": sinks,
    }
)

# Extract and emit events using rules
for sse_event in return_sink:
    for rule_name, rule in config.extraction.rules.items():
        if matches(sse_event, rule.match):
            data = extract(sse_event, rule.extract)
            
            if rule.emit.criticality == "droppable":
                emit_droppable(transcript, thread_id, rule.emit.event, data)
            else:
                transcript.write_event(thread_id, rule.emit.event, data)
```

## Project Overrides

```yaml
# .ai/config/streaming.yaml
extends: "rye/agent/threads/config/streaming.yaml"

streaming:
  sinks:
    definitions:
      websocket_ui:
        url: "wss://my-custom-ui.example.com/events"
        compression:
          enabled: true
          type: "gzip"
          
  extraction:
    rules:
      text_delta:
        throttle: "500ms"           # Faster UI updates
        
  tool_parsing:
    batch:
      size_threshold: 3             # Execute tools sooner
      delay_seconds: 0.05
```
