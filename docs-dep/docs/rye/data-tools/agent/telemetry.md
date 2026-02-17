**Source:** Implementation: `.ai/tools/rye/telemetry/` in rye-lilux

# Telemetry Category

## Purpose

Telemetry tools provide **opt-in execution tracking** for RYE operations with centralized storage that keeps item files clean.

**Location:** `.ai/tools/rye/telemetry/`
**Count:** 6 tools
**Executor:** All use `python_runtime`

## Key Principles

| Principle | Description |
|-----------|-------------|
| **Opt-in** | Default: off. Must explicitly enable via `configure.py` |
| **No git churn** | Stats stored centrally, not in item files |
| **Privacy-safe** | Telemetry never stored in repos |
| **Items unchanged** | Execution leaves item files unmodified |

## Storage Design

### Centralized Storage

```
~/.local/state/rye/telemetry.yaml    # XDG-compliant stats storage
~/.ai/config.yaml                     # Config with telemetry.enabled flag
```

### Cross-Platform Paths (via `get_rye_state_dir()`)

| Platform | State Directory |
|----------|-----------------|
| Linux | `$XDG_STATE_HOME/rye/` → `~/.local/state/rye/` |
| macOS | `~/Library/Application Support/rye/` |
| Windows | `%LOCALAPPDATA%/rye/` |

### Atomic Writes

- File locking with `fcntl.flock()` for concurrent access
- Write to `.tmp` file, then rename (atomic)
- Lock file: `telemetry.lock`

## Telemetry Tools

### 1. Shared Library (`lib.py`)

**Purpose:** Core telemetry infrastructure shared by all tools

```python
class TelemetryStore:
    """Manages centralized telemetry storage."""
    
    def record_execution(
        self,
        item_id: str,
        item_type: str,        # "directive" | "tool" | "knowledge"
        outcome: str,          # "success" | "failure" | "timeout" | "cancelled"
        duration_ms: float,
        http_calls: int = 0,
        subprocess_calls: int = 0,
        error: Optional[str] = None,
        path: Optional[str] = None,
    ): ...
    
    def get(self, item_id: str) -> Optional[dict]: ...
    def clear(self, item_id: Optional[str] = None): ...
```

### 2. Configure (`configure.py`)

**Purpose:** Enable or disable telemetry collection

```python
def configure_telemetry(enabled: bool) -> dict:
    """Enable or disable telemetry.
    
    Updates ~/.ai/config.yaml with telemetry.enabled flag.
    """
```

**Usage:**
```bash
python configure.py --enabled true   # Enable telemetry
python configure.py --enabled false  # Disable telemetry
```

### 3. Status (`status.py`)

**Purpose:** View telemetry configuration and stats

```python
def telemetry_status(item_id: Optional[str] = None) -> dict:
    """Get telemetry status and stats.
    
    Args:
        item_id: Specific item to show, or None for summary
    """
```

**Usage:**
```bash
python status.py                      # Summary of all items
python status.py --item-id my_tool    # Stats for specific item
```

### 4. Clear (`clear.py`)

**Purpose:** Clear telemetry data

```python
def clear_telemetry(item_id: Optional[str] = None) -> dict:
    """Clear telemetry stats.
    
    Args:
        item_id: Specific item to clear, or None for all
    """
```

**Usage:**
```bash
python clear.py                       # Clear all telemetry
python clear.py --item-id my_tool     # Clear specific item
```

### 5. Export (`export.py`)

**Purpose:** Bake telemetry stats into item frontmatter before publishing

```python
def export_telemetry(item_id: str, item_path: str) -> dict:
    """Bake telemetry stats into item frontmatter.
    
    One-time explicit action before publishing.
    Adds telemetry section to item's YAML frontmatter.
    """
```

**Exported Fields:**
```yaml
telemetry:
  total_runs: 42
  success_count: 40
  failure_count: 2
  success_rate: 0.952
  avg_duration_ms: 150.5
  last_run: "2026-01-30T12:00:00Z"
```

**Usage:**
```bash
python export.py --item-id my_tool --item-path .ai/tools/my_tool.py
```

### 6. Run With (`run_with.py`)

**Purpose:** Wrapper for execution with automatic telemetry tracking

```python
async def main(
    item_type: str,       # "directive" | "tool" | "knowledge"
    item_id: str,
    project_path: str,
    params: Optional[dict] = None,
):
    """Execute item with optional telemetry tracking.
    
    - Checks if telemetry is enabled
    - Records execution stats to TelemetryStore
    - Does not modify item files
    """
```

**Usage:**
```bash
python run_with.py \
  --item-type tool \
  --item-id my_tool \
  --project-path /home/user/project \
  --params '{"key": "value"}'
```

## TelemetryStore Fields

Each item tracked in `telemetry.yaml` has:

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | Item type: directive, tool, knowledge |
| `total_runs` | int | Total execution count |
| `success_count` | int | Successful executions |
| `failure_count` | int | Failed executions |
| `timeout_count` | int | Timed out executions |
| `avg_duration_ms` | float | Running average duration |
| `http_calls` | int | Total HTTP calls made |
| `subprocess_calls` | int | Total subprocess calls |
| `last_run` | string | ISO timestamp of last execution |
| `last_outcome` | string | Outcome of last execution |
| `last_error` | string | Error message if last failed |
| `paths` | list | File paths where item exists |

## Telemetry Data Flow

```
Execution (via run_with.py)
    │
    ├─→ Check config (telemetry.enabled?)
    │
    ├─→ Execute item
    │
    ├─→ Record stats to ~/.local/state/rye/telemetry.yaml
    │
    └─→ Item files remain unchanged
    
Before Publishing:
    │
    └─→ export.py bakes stats into item frontmatter
```

## Usage Examples

### Enable Telemetry

```bash
python .ai/tools/core/telemetry/configure.py --enabled true
```

### Check Status

```bash
python .ai/tools/core/telemetry/status.py
# Returns: config status, total items, last updated

python .ai/tools/core/telemetry/status.py --item-id my_tool
# Returns: detailed stats for specific item
```

### Export Before Publishing

```bash
python .ai/tools/core/telemetry/export.py \
  --item-id my_tool \
  --item-path .ai/tools/my_tool.py
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Count** | 6 tools |
| **Location** | `.ai/tools/core/telemetry/` |
| **Executor** | All use `python_runtime` |
| **Storage** | `~/.local/state/rye/telemetry.yaml` |
| **Config** | `~/.ai/config.yaml` |
| **Default** | Off (opt-in) |
| **Concurrency** | Atomic writes with file locking |

## Related Documentation

- [overview](../overview.md) - All categories
- [capabilities](capabilities.md) - System capabilities
- [../bundle/structure](../bundle/structure.md) - Bundle organization
