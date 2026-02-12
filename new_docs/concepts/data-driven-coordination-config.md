# Data-Driven Coordination Configuration

> Configuration for push-based thread coordination via asyncio.Event
>
> **Location:** `rye/rye/.ai/tools/rye/agent/threads/config/coordination.yaml`

## Overview

All coordination behavior is data-driven from YAML. No hardcoded constants for timeouts, event cleanup, or coordination policies.

## Configuration Schema

```yaml
# coordination.yaml
schema_version: "1.0.0"

coordination:
  # Wait threads configuration
  wait_threads:
    default_timeout: 600        # seconds (10 minutes)
    max_timeout: 3600           # seconds (1 hour hard limit)
    min_timeout: 1              # seconds
    
    # Polling fallback (only if event-driven fails)
    # Note: Should never happen with proper asyncio.Event usage
    polling_fallback:
      enabled: false            # Disabled by design
      interval: 0.1             # seconds (if ever enabled)
  
  # Completion events
  completion_events:
    # How long to keep events in memory after completion
    retention_minutes: 60
    
    # Cleanup interval for old events
    cleanup_interval_minutes: 30
    
    # Maximum events to track per process
    max_tracked_events: 10000
  
  # Active tasks tracking
  task_tracking:
    # Maximum concurrent tasks per process
    max_concurrent_tasks: 100
    
    # Task monitoring
    monitor_interval_seconds: 60
    
    # Detect hanging tasks
    hang_detection:
      enabled: true
      threshold_minutes: 30     # Warn if task running longer than this
  
  # Child thread coordination
  child_coordination:
    # Default wait mode
    default_mode: "all"         # "all" | "any"
    
    # Fail fast configuration
    fail_fast:
      default: false
      cancel_siblings_on_failure: false
    
    # Batch coordination (for wait_threads with many children)
    batch_wait:
      enabled: true
      max_concurrent_waits: 50  # Split large waits into batches

# Event Types
event_types:
  completion:
    description: "Thread reached terminal state"
    states: [completed, error, suspended, cancelled]
    
  checkpoint:
    description: "Thread saved state checkpoint"
    trigger: [pre_turn, post_llm, post_tools]
```

## Usage

```python
# Load coordination config
config = load_config("coordination.yaml", project_path)

# Use configured timeouts
async def wait_threads(thread_ids, timeout=None):
    timeout = timeout or config.wait_threads.default_timeout
    max_timeout = config.wait_threads.max_timeout
    actual_timeout = min(timeout, max_timeout)
    
    # Await completion events
    await asyncio.wait_for(
        gather_completion_events(thread_ids),
        timeout=actual_timeout
    )
```

## Project-Level Overrides

Projects can customize coordination:

```yaml
# .ai/config/coordination.yaml
extends: "rye/agent/threads/config/coordination.yaml"

coordination:
  wait_threads:
    default_timeout: 300      # Shorter timeout for this project
    
  child_coordination:
    fail_fast:
      default: true           # Always fail fast in this project
```
