# Data-Driven Thread System Architecture

> Complete data-driven architecture for Rye OS agent threads.
>
> **Location:** `rye/rye/.ai/tools/rye/agent/threads/`

## Overview

The thread system has been redesigned to be fully **data-driven** from YAML configuration, following the same patterns used by the core extractors (`EXTRACTION_RULES`, `VALIDATION_SCHEMA`).

## Documentation Index

### Core Configuration Schemas

1. **[data-driven-thread-events.md](data-driven-thread-events.md)** - Event type definitions
   - Replaces hardcoded `CRITICAL_TRANSCRIPT_EVENTS` / `DROPPABLE_TRANSCRIPT_EVENTS`
   - Defines: `cognition_in`, `cognition_out`, `cognition_out_delta`, `cognition_reasoning`, tool events, lifecycle events
   - Location: `rye/rye/.ai/tools/rye/agent/threads/config/events.yaml`

2. **[data-driven-error-classification.md](data-driven-error-classification.md)** - Error patterns & retry policies
   - Replaces procedural `classify_error()` with hardcoded if/elif chains
   - JSON Path matching operators: `eq`, `gt`, `in`, `regex`, `exists`
   - Location: `rye/rye/.ai/tools/rye/agent/threads/config/error_classification.yaml`

3. **[data-driven-hooks.md](data-driven-hooks.md)** - Hook condition evaluation
   - Replaces custom `when="..."` expression parser
   - JSON Path conditions with combinators: `all`, `any`, `not`
   - Location: `rye/rye/.ai/tools/rye/agent/threads/config/hook_conditions.yaml`

### Feature Configuration Schemas

4. **[data-driven-coordination-config.md](data-driven-coordination-config.md)** - Push-based coordination
   - Replaces hardcoded timeouts and event retention
   - Config: `wait_threads` timeouts, event cleanup, task tracking
   - Location: `rye/rye/.ai/tools/rye/agent/threads/config/coordination.yaml`

5. **[data-driven-resilience-config.md](data-driven-resilience-config.md)** - Retry & recovery
   - Replaces hardcoded retry policies (exponential backoff formulas)
   - Config: checkpoint intervals, budget limits, escalation rules
   - Location: `rye/rye/.ai/tools/rye/agent/threads/config/resilience.yaml`

6. **[data-driven-streaming-config.md](data-driven-streaming-config.md)** - HTTP streaming & sinks
   - Replaces hardcoded SSE parsing and sink configurations
   - Config: WebSocket sinks, extraction rules, tool parsing
   - Location: `rye/rye/.ai/tools/rye/agent/threads/config/streaming.yaml`

### Implementation Plans

8. **[data-driven-thread-implementation.md](data-driven-thread-implementation.md)** - Migration strategy
   - Refactoring strategy to move from hardcoded to config-driven
   - File organization and component architecture
   - Migration phases

### Streaming & Execution

5. **[thread-streaming-architecture.md](thread-streaming-architecture.md)** - Complete streaming pipeline
   - HTTP primitive SSE streaming with sink fan-out
   - WebSocket → UI, File → audit, ReturnSink → directive
   - Partial cognition preservation on error
   - Provider configuration for Anthropic/OpenAI

6. **[thread-streaming-execution.md](thread-streaming-execution.md)** - Inline tool execution
   - StreamingToolParser accumulates partial tool calls
   - Tools execute mid-stream (not after response)
   - Batching: 100ms delay or 5 tools
   - Interleaved transcript events

7. **[thread-coordination-events.md](thread-coordination-events.md)** - Push-based coordination
   - Replaces polling with `asyncio.Event`
   - `wait_threads` blocks on completion events
   - Zero latency, zero token cost
   - No transcript polling for coordination

### Related Documents

- **[agent-transcript-telemetry.md](agent-transcript-telemetry.md)** - Current transcript implementation
- **[agent-threads-future.md](agent-threads-future.md)** - Multi-turn, async, cross-thread features
- **[cognition-event-types-impl-plan.md](cognition-event-types-impl-plan.md)** - Event type migration plan
- **[thread-orchestration-internals.md](thread-orchestration-internals.md)** - Five gaps implementation plan
- **[thread-resilience-and-recovery.md](thread-resilience-and-recovery.md)** - Retry, checkpoint, recovery

## Key Changes

### Before (Hardcoded)

```python
# thread_directive.py
CRITICAL_EVENTS = {"thread_start", "cognition_out", "tool_call_start"}
DROPPABLE_EVENTS = {"cognition_out_delta", "cognition_reasoning"}

async def classify_error(error):
    if isinstance(error, RateLimitError):
        return "rate_limited"
    elif "429" in str(error):
        return "rate_limited"
    # ... more hardcoded patterns

def evaluate_hooks(event, context, hooks):
    for hook in hooks:
        if eval(hook.when):  # Custom parser!
            return hook.action
```

### After (Data-Driven)

```python
# thread_directive.py
emitter = EventEmitter.from_config(project_path)
emitter.emit(thread_id, "cognition_out", {"text": response})
# ^^ Uses events.yaml to determine: critical vs droppable, schema validation

classifier = ErrorClassifier.from_config(project_path)
classification = classifier.classify(error, context)
# ^^ Uses error_classification.yaml patterns

evaluator = HookEvaluator.from_config(project_path)
action = evaluator.evaluate(event_type, context, hooks)
# ^^ Uses hook_conditions.yaml operators
```

## Configuration Files

### System Defaults

```
rye/rye/.ai/tools/rye/agent/threads/config/
├── events.yaml              # Event type definitions
├── error_classification.yaml # Error patterns
├── hook_conditions.yaml     # Built-in hook templates
└── thread_modes.yaml        # Thread mode definitions
```

### Project Overrides

Projects can customize via `.ai/config/`:

```yaml
# .ai/config/thread_events.yaml
extends: "rye/agent/threads/config/events.yaml"

event_types:
  my_custom_event:
    category: custom
    criticality: droppable
    payload_schema:
      type: object
      required: [message]
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  thread_directive.py (orchestrator)                         │
├─────────────────────────────────────────────────────────────┤
│  EventEmitter ──► uses ──► events.yaml                      │
│  ErrorClassifier ──► uses ──► error_classification.yaml     │
│  HookEvaluator ──► uses ──► hook_conditions.yaml            │
└─────────────────────────────────────────────────────────────┘
                           │
        ┌──────────────────┼──────────────────┐
        ▼                  ▼                  ▼
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│  events.yaml │  │  error_      │  │  hook_       │
│              │  │  classifi-   │  │  conditions  │
│              │  │  cation.yaml │  │  .yaml       │
└──────────────┘  └──────────────┘  └──────────────┘
```

## Benefits

1. **No Magic Constants** - All behavior defined in YAML
2. **Project Customization** - Override at project level
3. **Schema Validation** - JSON Schema for all configs
4. **Testability** - Swap configs for testing
5. **Documentation** - Configs are self-documenting
6. **Consistency** - Same pattern as core extractors

## Migration Path

1. ✅ **Design** - Create config schemas (done)
2. ⏳ **Phase 1** - Create config files with current values
3. ⏳ **Phase 2** - Build config loaders
4. ⏳ **Phase 3** - Refactor components
5. ⏳ **Phase 4** - Remove hardcoded constants

## Consistent Vocabulary

| Old Term              | New Term              | Rationale                   |
| --------------------- | --------------------- | --------------------------- |
| `user_message`        | `cognition_in`        | Input to LLM, not "user"    |
| `assistant_text`      | `cognition_out`       | LLM output, not "assistant" |
| `assistant_reasoning` | `cognition_reasoning` | Thinking/CoT                |
| `thread_text`         | `cognition_out`       | Unified naming              |
| `paused`              | `suspended`           | Standard status term        |
| `awaiting: user`      | `awaiting: input`     | Generic input source        |

## Next Steps

1. Review and approve config schemas
2. Create system default config files
3. Implement config loaders
4. Refactor thread_directive.py incrementally
5. Update tests to use config-driven approach

9. **[data-driven-state-persistence.md](data-driven-state-persistence.md)** - State checkpoints (state.json)
   - Replaces inline state.json descriptions
   - Config: checkpoint triggers (pre_turn, post_llm, on_suspend), schema validation
   - Location: `rye/rye/.ai/tools/rye/agent/threads/config/state_schema.yaml`

10. **[data-driven-budget-ledger.md](data-driven-budget-ledger.md)** - Hierarchical budget tracking
   - SQLite schema for budget_ledger table
   - Config: transactions, operations (reserve, report_actual), cleanup
   - Location: `rye/rye/.ai/tools/rye/agent/threads/config/budget_ledger_schema.yaml`

## Tool-Level CONFIG_SCHEMA (Not System Config)

These are tool-specific settings defined in each tool's Python code via `CONFIG_SCHEMA`, not YAML files:

- **`parallel_dispatch` tool** - `max_concurrent_groups` (default: 25)
- **`StreamingToolParser` tool** - `batch_size` (default: 5), `batch_delay` (default: 0.1s)
- **`managed_subprocess` tool** - `max_processes`, `output_buffer_lines`

These are configured when the tool is called, not via system-wide YAML.
