**Source:** Original implementation: `.ai/tools/rye/sinks/` in kiwi-mcp

# Sinks Category

## Purpose

Sinks are **event destinations** where RYE directs execution results, logs, and events.

**Location:** `.ai/tools/rye/sinks/`
**Count:** 3 tools
**Executor:** All use `python_runtime`
**Protected:** ✅ Yes (core tool - cannot be shadowed)

## Core Sink Tools

### 1. File Sink (`file_sink.py`)

**Purpose:** Write events to files

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "sinks"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "path": {"type": "string", "description": "Output file path"},
        "mode": {"type": "string", "enum": ["write", "append"], "default": "append"},
        "format": {"type": "string", "enum": ["json", "text", "csv"], "default": "json"},
        "create_dirs": {"type": "boolean", "default": True},
    },
    "required": ["path"]
}
```

**Usage:**
- Log execution results to file
- Archive events
- Store audit trails

### 2. Null Sink (`null_sink.py`)

**Purpose:** Discard events (do nothing)

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "sinks"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "verbose": {"type": "boolean", "default": False},
    },
}
```

**Usage:**
- Disable event output
- Testing
- Performance testing (no I/O)

### 3. WebSocket Sink (`websocket_sink.py`)

**Purpose:** Send events to WebSocket endpoint

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "sinks"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "url": {"type": "string", "description": "WebSocket URL"},
        "format": {"type": "string", "enum": ["json", "msgpack"], "default": "json"},
        "reconnect": {"type": "boolean", "default": True},
        "timeout": {"type": "integer", "default": 30},
    },
    "required": ["url"]
}
```

**Usage:**
- Real-time event streaming
- Remote logging
- Live monitoring dashboards

## Event Flow

```
RYE Operations
    │
    └─→ Generate Events
        │
        ├─→ Execution results
        ├─→ Logs
        ├─→ Metrics
        └─→ Errors
            │
            ├─→ Route to Sinks
            │   ├─ File Sink → /path/to/file
            │   ├─ WebSocket Sink → ws://endpoint
            │   └─ Null Sink → discard
            │
            └─→ Event is processed
```

## Metadata Pattern

All sinks follow this pattern:

```python
# .ai/tools/rye/sinks/{name}.py

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "sinks"

CONFIG_SCHEMA = { ... }

def main(**kwargs) -> dict:
    """Process event/write to sink."""
    pass
```

## Usage Examples

### File Sink

```bash
Call file_sink with:
  path: "/var/log/rye/events.json"
  mode: "append"
  format: "json"
  create_dirs: true
```

### WebSocket Sink

```bash
Call websocket_sink with:
  url: "ws://monitoring-server:8080/events"
  format: "json"
  reconnect: true
  timeout: 30
```

### Null Sink (Disable Output)

```bash
Call null_sink with:
  verbose: false
```

## Common Sink Patterns

### Pattern 1: Local File Logging

```
Execution → File Sink → /var/log/rye/events.json
```

**Use Case:** Audit trail, local debugging, compliance logging

### Pattern 2: Remote Monitoring

```
Execution → WebSocket Sink → Monitoring Dashboard
```

**Use Case:** Live monitoring, real-time dashboards, alerting

### Pattern 3: No Output (Testing)

```
Execution → Null Sink → Discarded
```

**Use Case:** Performance testing, silent execution, test runs

## Event Schema

Events typically follow this structure:

```json
{
  "timestamp": "2026-01-30T12:00:00Z",
  "event_type": "execution",
  "tool": "git",
  "status": "success",
  "result": {
    "stdout": "...",
    "stderr": "",
    "returncode": 0
  },
  "duration_ms": 150,
  "metadata": {
    "request_id": "req-123",
    "user": "..."
  }
}
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Count** | 3 tools |
| **Location** | `.ai/tools/rye/sinks/` |
| **Executor** | All use `python_runtime` |
| **Purpose** | Direct events to destinations |
| **Outputs** | Files, WebSocket, null |

## Related Documentation

- [overview](overview.md) - All categories
- [agent/telemetry](../agent/telemetry.md) - Telemetry and monitoring
- [../bundle/structure](../bundle/structure.md) - Bundle organization
