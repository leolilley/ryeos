# Thread System v2 — Full Implementation Plan

> **Target directory:** `rye/rye/.ai/tools/rye/agent/threads/`
> **Backup of old code:** `rye/rye/.ai/tools/rye/agent/threads_backup_20260213/`
> **Architecture:** Data-Driven from YAML Configuration with Primary Tool Execution

---

## Table of Contents

1. [Architecture Overview](#1-architecture-overview)
2. [Module Dependency Graph](#2-module-dependency-graph)
3. [Execution Model: Sync vs Async](#3-execution-model-sync-vs-async)
4. [Configuration Schema](#4-configuration-schema)
5. [Internal Tools](#5-internal-tools)
6. [Module Specifications](#6-module-specifications)
7. [Cross-Cutting Concerns](#7-cross-cutting-concerns)
8. [Testing Strategy](#8-testing-strategy)
9. [Migration Checklist](#9-migration-checklist)

---

## 1. Architecture Overview

### Core Principle: Threads Use the Same Architecture as Everything Else

The thread system is **not special**. It follows the exact same data-driven patterns already established in RYE's core:

- **4 Primary Tools** — `search`, `load`, `execute`, `sign` — work on all 3 item types (`directive`, `tool`, `knowledge`)
- **YAML Extractors** — `directive_extractor.yaml`, `tool_extractor.yaml`, `knowledge_extractor.yaml` define validation schemas, extraction rules, search fields, and parser routing
- **YAML Primitives/Runtimes** — `subprocess.yaml`, `python_runtime.yaml`, etc. define execution config
- **Parsers** — `markdown_xml`, `markdown_frontmatter`, `python_ast`, `yaml` — routed by `ParserRouter`
- **3-Tier Space** — project > user > system for all discovery (tools, extractors, parsers, configs)

Thread-specific behavior is driven from **thread config YAML files**, loaded by thin loaders, and executed via the same primary tools. Hook actions are just `execute`/`search`/`load`/`sign` calls — the same 4 actions the LLM uses for everything else.

```
┌──────────────────────────────────────────────────────────────────────────┐
│  RYE Core (already exists)                                               │
├──────────────────────────────────────────────────────────────────────────┤
│  server.py  →  4 MCP tools: search, load, execute, sign                 │
│  tools/     →  SearchTool, LoadTool, ExecuteTool, SignTool               │
│  executor/  →  PrimitiveExecutor (chain: tool → runtime → primitive)     │
│  utils/     →  ParserRouter, validators, extensions, signature_formats   │
│                                                                          │
│  .ai/tools/rye/core/                                                     │
│    extractors/  → directive_extractor.yaml, tool_extractor.yaml,         │
│                   knowledge_extractor.yaml                               │
│    parsers/     → markdown_xml.py, python_ast.py, yaml.py, etc.          │
│    primitives/  → subprocess.yaml, http_client.yaml                      │
│    runtimes/    → python_runtime.yaml, node_runtime.yaml, etc.           │
└──────────────────────────────────────────────────────────────────────────┘
                               ↓ threads use all of this
┌──────────────────────────────────────────────────────────────────────────┐
│  Thread System (new — .ai/tools/rye/agent/threads/)                      │
├──────────────────────────────────────────────────────────────────────────┤
│  config/    → events.yaml, error_classification.yaml, resilience.yaml,   │
│               hook_conditions.yaml, budget_ledger_schema.yaml            │
│  loaders/   → Config loaders (YAML + extends + project override)         │
│  internal/  → Thin tools callable via rye_execute                        │
│  Core files → thread_directive.py, runner.py, orchestrator.py,           │
│               safety_harness.py                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

### How It Connects to Core

| Thread Need         | Core Component Used                 | How                                                                  |
| ------------------- | ----------------------------------- | -------------------------------------------------------------------- |
| Parse directive XML | `ParserRouter` → `markdown_xml.py`  | Extracts flat `actions` list + `body`/`content`                      |
| Validate directive  | `validators.validate_parsed_data()` | Uses `directive_extractor.yaml` schema (at sign time)                |
| Execute hook action | `ToolDispatcher` → `ExecuteTool`    | Translates action `params` → tool `parameters`, injects project_path |
| Search for tools    | `ToolDispatcher` → `SearchTool`     | Same translation layer, routes by `primary`                          |
| Load items          | `ToolDispatcher` → `LoadTool`       | Same translation layer, routes by `primary`                          |
| Sign internal tools | `SignTool.handle()`                 | Same as any `rye_sign` call                                          |
| Discover extensions | `extensions.get_tool_extensions()`  | From `tool_extractor.yaml`                                           |
| Resolve tool chain  | `PrimitiveExecutor`                 | tool → runtime → primitive chain                                     |

### Parser: Actions Not Steps

The `markdown_xml` parser scans the **entire XML tree** (excluding `<metadata>` internals) for the four primary action tags. It does not require `<process>` or `<step>` structure — directive body is freeform. Action tags can appear anywhere:

```python
from rye.constants import Action

PRIMARY_ACTIONS = frozenset(Action.ALL)  # {"search", "load", "execute", "sign"}

# Parser scans entire tree for tags matching PRIMARY_ACTIONS and produces:
parsed["actions"] = [
    {"primary": "execute", "item_type": "tool", "item_id": "...", "params": {...}},
    {"primary": "search", "query": "...", "source": "..."},
    {"primary": "load", "item_type": "directive", "item_id": "..."},
]
```

All action tags use the same attribute pass-through — `action.update(elem.attrib)` — no special-casing for `execute` vs others. Tags inside `<metadata>` (e.g., `<execute>*</execute>` in permissions) are excluded.

Thread system code uses `directive.get("actions")` — never `directive.get("steps")`.

### `body` vs `content`

Both are extracted by the parser:

- **`body`** — markdown text before the `\`\`\`xml` fence, used as the **user prompt**
- **`content`** — the raw XML string, used for **reference or dry-run display**

### Canonical Limit Field Names

No `max_` prefix anywhere. The canonical fields are:

| Field              | Type  | Default |
| ------------------ | ----- | ------- |
| `turns`            | int   | 25      |
| `tokens`           | int   | 4096    |
| `spend`            | float | 1.0     |
| `spend_currency`   | str   | "USD"   |
| `spawns`           | int   | 10      |
| `duration_seconds` | int   | 600     |

These names are used in: directive `<limits>` tags, `resilience.yaml` defaults, `DEFAULT_LIMITS` constants, and all limit-checking code.

### Hook Actions Follow the Same 4-Action Pattern

Hooks define actions using the **exact same format** as directive `<execute>`, `<search>`, `<load>`, `<sign>` tags:

```yaml
# hook_conditions.yaml — hook actions are primary tool calls
action:
  primary: "execute" # One of: execute, search, load, sign
  item_type: "tool" # One of: directive, tool, knowledge
  item_id: "rye/agent/threads/internal/control"
  params:
    action: "retry"
```

The `ToolDispatcher` (§6.7) translates these action dicts into the core `ExecuteTool`/`SearchTool`/`LoadTool`/`SignTool` `handle()` kwargs — mapping `params` → `parameters`, injecting `project_path`, and routing by `primary`.

### Project Overrides

Projects can override thread system defaults via `.ai/config/`:

```yaml
# .ai/config/error_classification.yaml
extends: "rye/agent/threads/config/error_classification.yaml"

patterns:
  - id: "custom_api_down"
    category: "transient"
    match:
      path: "error.code"
      op: "eq"
      value: "MAINTENANCE_MODE"
    retry_policy:
      type: "fixed"
      delay: 300.0
```

### What the Old System Had Wrong

1. Hardcoded patterns in Python if/elif chains
2. Config-consumer classes (`ThreadRuntime`, `ThreadResilience`) with unclear ownership
3. No project-level customization
4. Hook actions as special cases, not primary tool calls
5. Redefined operators/matching logic instead of using config-driven evaluation

### What the New System Does

1. **All behavior from YAML** — patterns, limits, events, hooks all in config
2. **Loaders handle overrides** — system defaults + project overrides merged
3. **Tools are thin wrappers** — execute logic defined in config
4. **Primary tool execution** — hooks use the same 4 actions as everything else
5. **Extractors/parsers/validators from core** — no redefinition, just use them

---

## 2. Module Dependency Graph

```
rye/rye/.ai/tools/rye/agent/threads/
│
├── thread_directive.py     # Entry point (~400 lines)
├── runner.py               # Core LLM loop (~500 lines)
├── orchestrator.py         # Thread coordination (~400 lines)
├── safety_harness.py       # Thin facade (~300 lines)
│
├── loaders/                # Config loaders + shared utilities
│   ├── config_loader.py    # Base: YAML load + extends + merge-by-id
│   ├── condition_evaluator.py # Shared path/op/value + combinators
│   ├── interpolation.py    # ${...} template engine
│   ├── events_loader.py    # Load events.yaml
│   ├── error_loader.py     # Load error_classification.yaml
│   ├── hooks_loader.py     # Load hook_conditions.yaml
│   └── resilience_loader.py # Load resilience.yaml
│
├── config/                 # System default YAML configs
│   ├── events.yaml
│   ├── error_classification.yaml
│   ├── hook_conditions.yaml
│   ├── resilience.yaml
│   └── budget_ledger_schema.yaml
│
├── adapters/               # Provider/tool adaptation
│   ├── provider_adapter.py     # Abstract LLM provider interface
│   └── tool_dispatcher.py      # Translate action dicts → core tool handle() kwargs
│
├── persistence/            # State/storage
│   ├── state_store.py          # Atomic state.json persistence
│   ├── budgets.py              # SQLite budget ledger
│   └── thread_registry.py      # Thread registry DB
│
├── events/                 # Event handling
│   ├── event_emitter.py        # Emit events (criticality routing)
│   └── streaming_tool_parser.py # Parse streaming chunks
│
├── security/               # Security
│   └── security.py             # Capability tokens, redaction
│
└── internal/               # Tools callable via primary tool execution
    ├── control.py              # retry/fail/abort/continue/escalate/suspend
    ├── emitter.py              # Emit events to transcript
    ├── classifier.py           # Classify errors (uses error_classification.yaml)
    ├── limit_checker.py        # Check limits (uses resilience.yaml)
    ├── state_persister.py      # Persist harness state
    ├── cancel_checker.py       # Check cancellation requests
    ├── budget_ops.py           # reserve/report/release budget
    └── cost_tracker.py         # Track LLM costs
```

### Import Pattern

Thread modules use two import mechanisms:

**Sibling imports** — for modules within the thread system (`.ai/tools/` are not installed packages):

```python
import importlib.util
from pathlib import Path as PathLib

def _load_sibling(relative_path: str):
    """Load a sibling module by relative path from this file's directory."""
    path = PathLib(__file__).parent / relative_path
    if not path.suffix:
        path = path.with_suffix(".py")
    # Sanitize module name: "loaders/config_loader" → "loaders.config_loader"
    module_name = relative_path.replace("/", ".").removesuffix(".py")
    spec = importlib.util.spec_from_file_location(module_name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

config_loader = _load_sibling("loaders/config_loader")
```

**Package imports** — for the installed `rye` package (constants, utils, tools):

```python
from rye.constants import Action, ItemType
from rye.utils.parser_router import ParserRouter
from rye.tools.execute import ExecuteTool
```

### Runtime Requirement

Thread tools require `rye` to be importable in the Python interpreter used by `python_runtime.yaml`. In development this means the venv at `${RYE_PYTHON}` has `rye` installed (typically via `pip install -e .`). The `python_runtime.yaml` env config resolves this via the `venv_path: .venv` setting.

### What the Thread System Does NOT Redefine

These already exist in core and the thread system **uses them directly**:

| Capability               | Core Location                                   | Used By                                  |
| ------------------------ | ----------------------------------------------- | ---------------------------------------- |
| XML parsing              | `parsers/markdown_xml.py`                       | `thread_directive.py` via `ParserRouter` |
| Validation schemas       | `extractors/directive/directive_extractor.yaml` | Validators at sign time                  |
| Field extraction rules   | `extractors/*/`                                 | `validators.apply_field_mapping()`       |
| Tool execution chains    | `executor/primitive_executor.py`                | `ExecuteTool._run_tool()`                |
| Search with BM25 scoring | `tools/search.py`                               | Any hook that does `primary: "search"`   |
| Integrity verification   | `utils/integrity.py`                            | `verify_item()` on load/execute          |
| Signature formats        | `utils/signature_formats.py`                    | From extractor YAML                      |

---

## 3. Execution Model: Sync vs Async

### Three Patterns

| Pattern               | Directive Process                                                                                        | Mechanism                                                  |
| --------------------- | -------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| **Sync**              | `<execute item_id="thread_directive">` (no async_exec)                                                   | `await runner.run()` — blocks until complete               |
| **Fire-and-forget**   | `<execute item_id="thread_directive"><param name="async_exec" value="true"/></execute>`                  | Spawn `runner.run()` as `asyncio.Task`, return immediately |
| **Fan-out + collect** | N async spawns + `<execute item_id="orchestrator"><param name="action" value="wait_threads"/></execute>` | N spawns + 1 `wait_threads` call                           |

### Wave-Based Orchestration Example

The LLM executes **directives**. The directive's `<process>` contains freeform prose steps for the LLM, with deterministic `<execute>`, `<search>`, `<sign>`, `<load>` tags that the parser extracts as a flat `actions` list:

```xml
<!-- Directive: plan_db.md -->
<directive name="plan_db">
  <process>
    <step name="spawn_async_thread">
      <description>Spawn async thread to plan database schema</description>
      <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
        <param name="directive_name" value="internal/db_planner" />
        <param name="async_exec" value="true" />
        <param name="inputs" value='{"scope": "${input:scope}"}' />
      </execute>
    </step>
  </process>
</directive>
```

The parser produces from this:

```python
parsed["actions"] = [
    {
        "primary": "execute",
        "item_type": "tool",
        "item_id": "rye/agent/threads/thread_directive",
        "params": {
            "directive_name": "internal/db_planner",
            "async_exec": "true",
            "inputs": '{"scope": "${input:scope}"}',
        },
    }
]
```

**Multi-turn flow:**

```
Turn 1: LLM executes directive scaffold_project
        → directive process instructs LLM to call thread_directive (sync)
        → LLM calls thread_directive → blocks → returns full result

Turn 2: LLM executes directive plan_db
        → directive process instructs LLM to call thread_directive(async_exec=true)
        → LLM calls thread_directive → returns {"thread_id": "A", ...}

        LLM executes directive plan_api
        → directive process instructs LLM to call thread_directive(async_exec=true)
        → LLM calls thread_directive → returns {"thread_id": "B", ...}

Turn 3: LLM executes directive collect_results
        → directive process instructs LLM to call orchestrator.wait_threads
        → LLM calls orchestrator with thread_ids=["A", "B"]
        → blocks until both complete → returns aggregated results
```

### Async Return Payload Schema

When `async_exec=true`, `thread_directive.execute()` returns immediately:

```python
ASYNC_RETURN_PAYLOAD = {
    "success": True,
    "thread_id": str,                   # Generated thread ID
    "status": "running",
    "directive": str,                   # Directive name
    "control": {
        "wait": "orchestrator.wait_threads(['{thread_id}'])",
        "cancel": "state_store.request_cancel('{thread_id}')",
        "status": "registry.get_status('{thread_id}')",
    },
}
```

### Thread Lifecycle State Machine

```
        ┌──────────┐
        │  created │
        └────┬─────┘
             │ execute()
             ▼
        ┌──────────┐
     ┌──│ running  │◄─────────────┐
     │  └────┬─────┘              │
     │       │                    │
     │  ┌────┴────┐               │
     │  │         │               │
     │  ▼         ▼               │
┌────┴────┐ ┌─────┴─────┐         │
│completed│ │  error    │         │
└─────────┘ └─────┬─────┘         │
                  │               │
                  ▼               │
            ┌──────────┐          │
            │ suspended│──────────┘ resume()
            └────┬─────┘
                 │ cancel()
                 ▼
            ┌──────────┐
            │cancelled │
            └──────────┘
```

---

## 4. Configuration Schema

### 4.1 events.yaml

Defines all thread transcript events.

```yaml
# config/events.yaml
schema_version: "1.0.0"

event_types:
  # Lifecycle Events
  thread_started:
    category: lifecycle
    criticality: critical
    description: "Thread execution begins"
    payload_schema:
      type: object
      required: [directive, model]
      properties:
        directive: { type: string }
        model: { type: string }
        limits: { type: object }

  thread_completed:
    category: lifecycle
    criticality: critical
    payload_schema:
      type: object
      required: [cost]
      properties:
        cost: { type: object }

  thread_suspended:
    category: lifecycle
    criticality: critical
    payload_schema:
      type: object
      required: [suspend_reason]
      properties:
        suspend_reason: { type: string, enum: [limit, error, budget, approval] }

  thread_cancelled:
    category: lifecycle
    criticality: critical
    payload_schema:
      type: object
      properties:
        reason: { type: string }

  # Cognition Events
  cognition_in:
    category: cognition
    criticality: critical
    description: "Prompt sent to LLM"
    payload_schema:
      type: object
      required: [text, role]
      properties:
        text: { type: string }
        role: { type: string, enum: [system, user, developer] }
        directive: { type: string }

  cognition_out:
    category: cognition
    criticality: critical
    emit_on_error: true # Always emit partial on interruption
    payload_schema:
      type: object
      required: [text]
      properties:
        text: { type: string }
        model: { type: string }
        is_partial: { type: boolean, default: false }
        truncated: { type: boolean, default: false }
        error: { type: string }
        completion_percentage: { type: number, minimum: 0, maximum: 100 }

  cognition_out_delta:
    category: cognition
    criticality: droppable
    description: "Streaming text chunk"
    emit_config:
      async: true
      throttle_seconds: 0.1
    payload_schema:
      type: object
      required: [text]
      properties:
        text: { type: string }
        chunk_index: { type: integer }
        is_final: { type: boolean }

  cognition_reasoning:
    category: cognition
    criticality: droppable
    emit_on_error: true
    emit_config:
      accumulate_on_error: true
    payload_schema:
      type: object
      required: [text]
      properties:
        text: { type: string }
        is_partial: { type: boolean }
        was_interrupted: { type: boolean }

  # Tool Events
  tool_call_start:
    category: tool
    criticality: critical
    payload_schema:
      type: object
      required: [tool, call_id, input]
      properties:
        tool: { type: string }
        call_id: { type: string }
        input: { type: object }

  tool_call_result:
    category: tool
    criticality: critical
    payload_schema:
      type: object
      required: [call_id]
      properties:
        call_id: { type: string }
        output: { type: string }
        error: { type: string }
        duration_ms: { type: integer }

  # Error Events
  error_classified:
    category: error
    criticality: critical
    payload_schema:
      type: object
      required: [error_code, category]
      properties:
        error_code: { type: string }
        category: { type: string }
        retryable: { type: boolean }

  limit_escalation_requested:
    category: error
    criticality: critical
    payload_schema:
      type: object
      required: [limit_code]
      properties:
        limit_code: { type: string }
        current_value: { type: number }
        current_max: { type: number }

  # Orchestration Events
  child_thread_started:
    category: orchestration
    criticality: critical
    payload_schema:
      type: object
      required: [child_thread_id, child_directive]
      properties:
        child_thread_id: { type: string }
        child_directive: { type: string }
        parent_thread_id: { type: string }

# Criticality Levels
criticality_levels:
  critical:
    description: "Synchronous, blocking write"
    durability: guaranteed
    async: false

  droppable:
    description: "Fire-and-forget async emission"
    durability: best_effort
    async: true
    fallback: drop
```

### 4.2 error_classification.yaml

Error patterns and retry policies. Uses the same match condition format as hook conditions — `path`/`op`/`value` with `any`/`all`/`not` combinators.

```yaml
# config/error_classification.yaml
schema_version: "1.0.0"

patterns:
  # Rate Limiting
  - id: "http_429"
    name: "rate_limited"
    category: "rate_limited"
    retryable: true
    match:
      any:
        - path: "status_code"
          op: "eq"
          value: 429
        - path: "error.type"
          op: "in"
          value: ["rate_limit_error", "RateLimitError"]
        - path: "error.message"
          op: "regex"
          value: "rate limit|too many requests|throttled"
    retry_policy:
      type: "use_header"
      header: "retry-after"
      fallback:
        type: "exponential"
        base: 2.0
        max: 60.0

  # Transient Network Errors
  - id: "network_timeout"
    name: "transient_timeout"
    category: "transient"
    retryable: true
    match:
      any:
        - path: "error.type"
          op: "in"
          value: ["TimeoutError", "ReadTimeout", "ConnectTimeout"]
        - path: "error.message"
          op: "regex"
          value: "timeout|timed out"
    retry_policy:
      type: "exponential"
      base: 2.0
      max: 30.0

  - id: "network_connection"
    name: "transient_connection"
    category: "transient"
    retryable: true
    match:
      any:
        - path: "error.type"
          op: "in"
          value: ["ConnectionError", "ConnectionResetError"]
        - path: "error.message"
          op: "regex"
          value: "connection reset|connection refused|network"
    retry_policy:
      type: "exponential"
      base: 2.0
      max: 60.0

  - id: "http_5xx"
    name: "transient_server"
    category: "transient"
    retryable: true
    match:
      path: "status_code"
      op: "in"
      value: [500, 502, 503, 504]
    retry_policy:
      type: "exponential"
      base: 2.0
      max: 120.0

  # Permanent Errors (No Retry)
  - id: "auth_failure"
    name: "permanent_auth"
    category: "permanent"
    retryable: false
    match:
      any:
        - path: "status_code"
          op: "in"
          value: [401, 403]
        - path: "error.code"
          op: "in"
          value: ["authentication_error", "authorization_error"]

  - id: "validation_error"
    name: "permanent_validation"
    category: "permanent"
    retryable: false
    match:
      any:
        - path: "status_code"
          op: "eq"
          value: 422
        - path: "error.type"
          op: "eq"
          value: "ValidationError"

  # Cancellation
  - id: "cancelled"
    name: "cancelled"
    category: "cancelled"
    retryable: false
    match:
      any:
        - path: "error.type"
          op: "eq"
          value: "CancelledError"
        - path: "cancelled"
          op: "eq"
          value: true

# Default for unmatched errors
default:
  category: "permanent"
  retryable: false
  retry_policy:
    type: "none"

# Match Operators (same set used by hook conditions)
operators:
  eq: { description: "Equal" }
  ne: { description: "Not equal" }
  gt: { description: "Greater than" }
  gte: { description: "Greater than or equal" }
  lt: { description: "Less than" }
  lte: { description: "Less than or equal" }
  in: { description: "In list" }
  contains: { description: "String contains" }
  regex: { description: "Regex match" }
  exists: { description: "Path exists" }

# Combinators (same set used by hook conditions)
combinators:
  any: { description: "Match if any child matches" }
  all: { description: "Match only if all children match" }
  not: { description: "Match if child does not match" }

# Retry Policy Types
retry_policy_types:
  exponential:
    parameters:
      base: { type: number, default: 2.0 }
      max: { type: number, default: 120.0 }
    formula: "min(base * (2 ** attempt), max)"

  fixed:
    parameters:
      delay: { type: number, default: 60.0 }
    formula: "delay"

  use_header:
    parameters:
      header: { type: string, default: "retry-after" }
      fallback: { type: object }
    formula: "int(headers[header]) or fallback"

  none:
    parameters: {}
    formula: "null"
```

### 4.3 hook_conditions.yaml

Built-in hooks. Actions are primary tool calls — the same 4 actions (`execute`, `search`, `load`, `sign`) used everywhere in RYE.

```yaml
# config/hook_conditions.yaml
schema_version: "1.0.0"

# Layer 2: Built-in default hooks
builtin_hooks:
  - id: "default_retry_transient"
    event: "error"
    layer: 2
    condition:
      path: "classification.category"
      op: "in"
      value: ["transient", "rate_limited"]
    action:
      primary: "execute"
      item_type: "tool"
      item_id: "rye/agent/threads/internal/control"
      params:
        action: "retry"
    description: "Retry transient and rate-limited errors"

  - id: "default_fail_permanent"
    event: "error"
    layer: 2
    condition:
      path: "classification.category"
      op: "eq"
      value: "permanent"
    action:
      primary: "execute"
      item_type: "tool"
      item_id: "rye/agent/threads/internal/control"
      params:
        action: "fail"
        error: "${error.message}"
    description: "Fail on permanent errors"

  - id: "default_abort_cancelled"
    event: "error"
    layer: 2
    condition:
      path: "classification.category"
      op: "eq"
      value: "cancelled"
    action:
      primary: "execute"
      item_type: "tool"
      item_id: "rye/agent/threads/internal/control"
      params:
        action: "abort"
    description: "Abort on cancellation"

  - id: "default_escalate_limit"
    event: "limit"
    layer: 2
    condition:
      path: "limit_code"
      op: "in"
      value: ["spend_exceeded", "turns_exceeded", "tokens_exceeded"]
    action:
      primary: "execute"
      item_type: "tool"
      item_id: "rye/agent/threads/internal/control"
      params:
        action: "escalate"
        limit_type: "${limit_code}"
        current_value: "${current_value}"
    description: "Escalate limit hits for approval"

  - id: "default_context_compaction"
    event: "context_window_pressure"
    layer: 2
    condition:
      path: "pressure_ratio"
      op: "gte"
      value: 0.8
    action:
      primary: "execute"
      item_type: "directive"
      item_id: "rye/agent/threads/default_compaction"
      params:
        pressure_ratio: "${event.pressure_ratio}"
        tokens_used: "${event.tokens_used}"
    description: "Trigger context compaction"

# Layer 3: Infrastructure hooks (always run)
infra_hooks:
  - id: "infra_save_state"
    event: "after_step"
    layer: 3
    action:
      primary: "execute"
      item_type: "tool"
      item_id: "rye/agent/threads/internal/emitter"
      params:
        event_type: "checkpoint_saved"
        payload:
          turn: "${cost.turns}"

  - id: "infra_completion_signal"
    event: "after_complete"
    layer: 3
    action:
      primary: "execute"
      item_type: "tool"
      item_id: "rye/agent/threads/internal/emitter"
      params:
        event_type: "thread_completed"
        payload:
          cost: "${cost}"

# Action Types — all are primary tool calls
# These are the same 4 actions as rye.constants.Action.ALL
action_primaries: ["execute", "search", "load", "sign"]

# Item types — same as rye.constants.ItemType.ALL
action_item_types: ["directive", "tool", "knowledge"]
```

### 4.4 resilience.yaml

Retry policies, limits, checkpoints.

```yaml
# config/resilience.yaml
schema_version: "1.0.0"

retry:
  max_retries: 3

  policies:
    exponential:
      type: exponential
      base: 2.0
      max_delay: 120.0

    fixed:
      type: fixed
      delay: 60.0

    rate_limited:
      type: use_header
      header: "retry-after"
      fallback:
        policy: exponential
        base: 5.0

limits:
  defaults:
    turns: 25
    tokens: 4096
    spend: 1.0
    spend_currency: "USD"
    spawns: 10
    duration_seconds: 600

  enforcement:
    check_before_turn: true
    check_after_turn: true
    on_exceed: escalate

checkpoint:
  triggers:
    pre_turn: true
    post_llm: true
    post_tools: true
    on_error: true

  persistence:
    atomic_write: true
    format: json

coordination:
  wait_timeout_seconds: 300.0
  fail_fast: false
  cancel_siblings_on_failure: false
  max_wait_thread_ids: 50
  completion_event_ttl_seconds: 600

child_policy:
  on_parent_cancel: "cascade_cancel"
  on_parent_complete: "allow"
  on_parent_error: "cascade_cancel"

concurrency:
  max_concurrent_children: 5
  max_total_threads: 20
```

### 4.5 budget_ledger_schema.yaml

SQLite schema for budget tracking.

```yaml
# config/budget_ledger_schema.yaml
schema_version: "1.0.0"

table:
  name: "budget_ledger"
  columns:
    - name: thread_id
      type: TEXT
      primary_key: true

    - name: parent_thread_id
      type: TEXT
      nullable: true

    - name: reserved_spend
      type: REAL
      default: 0.0

    - name: actual_spend
      type: REAL
      default: 0.0

    - name: max_spend
      type: REAL
      nullable: true

    - name: status
      type: TEXT
      default: "active"

    - name: created_at
      type: TEXT

    - name: updated_at
      type: TEXT

  indexes:
    - name: idx_budget_parent
      columns: [parent_thread_id]
    - name: idx_budget_status
      columns: [status]

operations:
  reserve:
    isolation: "IMMEDIATE"
    check_remaining: true

  report_actual:
    clamp_to_reserved: true

  release:
    on_status: ["completed", "cancelled", "error"]

cleanup:
  archive_after_days: 30
  vacuum_interval_days: 7
```

---

## 5. Internal Tools

Internal tools live at `.ai/tools/rye/agent/threads/internal/` and are callable via `rye_execute` — the same primary `execute` tool that runs any other tool. They follow the standard tool contract: `__version__`, `__tool_type__`, `__category__`, `__tool_description__`, `CONFIG_SCHEMA`, `execute(params, project_path)`.

The `PrimitiveExecutor` resolves their execution chain like any other tool: internal tool → `python_runtime.yaml` → `subprocess.yaml`.

### 5.1 control.py

```python
# internal/control.py
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Handle thread control actions"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ["retry", "fail", "abort", "continue", "escalate", "suspend", "skip"],
        },
        "error": {"type": "string"},
        "limit_type": {"type": "string"},
        "current_value": {"type": "number"},
        "suspend_reason": {"type": "string"},
    },
    "required": ["action"],
}

def execute(params: Dict, project_path: str) -> Optional[Dict]:
    """Execute a control action.

    Returns None for continue/skip, or a result dict for terminating actions.
    The runner interprets the return value to determine flow control.
    """
    action = params.get("action", "continue")

    if action in ("continue", "skip"):
        return None

    if action == "retry":
        return {"action": "retry"}

    if action == "fail":
        return {"success": False, "error": params.get("error", "Hook triggered failure")}

    if action == "abort":
        return {"success": False, "aborted": True, "error": "Aborted by hook"}

    if action == "suspend":
        return {
            "success": False,
            "suspended": True,
            "error": params.get("suspend_reason", "Suspended by hook"),
        }

    if action == "escalate":
        return {
            "success": False,
            "suspended": True,
            "escalated": True,
            "error": "Escalation requested",
            "escalation": {
                "limit_type": params.get("limit_type"),
                "current_value": params.get("current_value"),
            },
        }

    return None
```

### 5.2 emitter.py

```python
# internal/emitter.py
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Emit transcript events"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "event_type": {"type": "string"},
        "payload": {"type": "object", "default": {}},
        "criticality": {"type": "string", "enum": ["critical", "droppable"]},
        "_thread_context": {"type": "object"},
    },
    "required": ["event_type"],
}

def execute(params: Dict, project_path: str) -> Dict:
    """Emit an event to the transcript."""
    event_type = params.get("event_type")
    payload = params.get("payload", {})
    criticality = params.get("criticality", "critical")

    ctx = params.get("_thread_context", {})
    emitter = ctx.get("emitter")
    transcript = ctx.get("transcript")
    thread_id = ctx.get("thread_id", "unknown")

    if not emitter or not transcript:
        return {"success": False, "error": "Missing thread context"}

    if criticality == "critical":
        emitter.emit_critical(thread_id, event_type, payload, transcript=transcript)
    else:
        emitter.emit(thread_id, event_type, payload, transcript=transcript)

    return {"success": True, "event_type": event_type, "emitted": True}
```

### 5.3 classifier.py

Uses `error_classification.yaml` via loader. The match evaluation logic (`_matches`, `_resolve_path`, `_apply_operator`) is the shared pattern used by both classifier and hooks.

```python
# internal/classifier.py
__version__ = "1.0.0"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Classify errors using config patterns"

def execute(params: Dict, project_path: str) -> Dict:
    """Classify an error using error_classification.yaml patterns."""
    from ..loaders import error_loader
    return error_loader.classify(project_path, {
        "error": params.get("error", {}),
        "status_code": params.get("status_code"),
        "headers": params.get("headers", {}),
    })
```

The classifier is a thin wrapper — all pattern evaluation lives in `error_loader.classify()`.

### 5.4 limit_checker.py

Uses `resilience.yaml` via loader.

```python
# internal/limit_checker.py
__version__ = "1.0.0"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Check thread limits"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "limit_type": {"type": "string"},
        "current_value": {"type": "number"},
        "max_value": {"type": "number"},
    },
    "required": ["limit_type", "current_value", "max_value"],
}

def execute(params: Dict, project_path: str) -> Dict:
    """Check if a limit is exceeded."""
    from ..loaders import resilience_loader
    config = resilience_loader.load(project_path)

    limit_type = params.get("limit_type")
    current = params.get("current_value")
    maximum = params.get("max_value")

    if current >= maximum:
        on_exceed = config.get("limits", {}).get("enforcement", {}).get("on_exceed", "fail")
        return {
            "success": True,
            "exceeded": True,
            "limit_type": limit_type,
            "current": current,
            "max": maximum,
            "action": on_exceed,
        }

    return {"success": True, "exceeded": False}
```

### 5.5 budget_ops.py

```python
# internal/budget_ops.py
__version__ = "1.0.0"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Budget operations"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {"type": "string", "enum": ["reserve", "report_actual", "release", "check_remaining"]},
        "thread_id": {"type": "string"},
        "parent_thread_id": {"type": "string"},
        "amount": {"type": "number"},
    },
    "required": ["operation", "thread_id"],
}

def execute(params: Dict, project_path: str) -> Dict:
    """Execute budget operation."""
    from ..persistence import budgets

    operation = params.get("operation")
    thread_id = params.get("thread_id")
    ledger = budgets.get_ledger(Path(project_path))

    if operation == "reserve":
        parent_id = params.get("parent_thread_id")
        amount = params.get("amount", 0.0)
        success = ledger.reserve(thread_id, amount, parent_thread_id=parent_id)
        return {"success": success, "reserved": amount if success else 0}

    if operation == "report_actual":
        amount = params.get("amount", 0.0)
        ledger.report_actual(thread_id, amount)
        return {"success": True, "reported": amount}

    if operation == "release":
        ledger.release(thread_id)
        return {"success": True, "released": True}

    if operation == "check_remaining":
        remaining = ledger.get_remaining(thread_id)
        return {"success": True, "remaining": remaining}

    return {"success": False, "error": f"Unknown operation: {operation}"}
```

---

## 6. Module Specifications

### 6.1 loaders/config_loader.py

Base config loader with extends support.

```python
# loaders/config_loader.py
from pathlib import Path
from typing import Dict, Any, Optional
import yaml

class ConfigLoader:
    """Base loader for YAML configs with extends support."""

    def __init__(self, config_name: str):
        self.config_name = config_name
        self._cache: Dict[str, Any] = {}

    def load(self, project_path: Path) -> Dict[str, Any]:
        """Load config with project overrides."""
        cache_key = str(project_path)
        if cache_key in self._cache:
            return self._cache[cache_key]

        # Load system default
        system_path = Path(__file__).parent.parent / "config" / self.config_name
        config = self._load_yaml(system_path)

        # Check for project override
        project_config_path = project_path / ".ai" / "config" / self.config_name
        if project_config_path.exists():
            project_config = self._load_yaml(project_config_path)
            config = self._merge(config, project_config)

        self._cache[cache_key] = config
        return config

    def _load_yaml(self, path: Path) -> Dict[str, Any]:
        with open(path) as f:
            return yaml.safe_load(f) or {}

    def _merge(self, base: Dict, override: Dict) -> Dict:
        """Deep merge override into base.

        Merge semantics:
        - `extends` key: skipped (metadata only)
        - Dicts: recursive deep merge
        - Lists of dicts with `id` keys: merge-by-id (match on `id` field,
          override matching entries, append new entries)
        - Lists without `id` keys: replace entirely
        - Scalars: replace
        """
        result = dict(base)
        for key, value in override.items():
            if key == "extends":
                continue
            if key in result and isinstance(result[key], dict) and isinstance(value, dict):
                result[key] = self._merge(result[key], value)
            elif (key in result
                  and isinstance(result[key], list) and isinstance(value, list)
                  and result[key] and isinstance(result[key][0], dict)
                  and result[key][0].get("id") is not None):
                # Merge-by-id: allows project overrides to modify specific hooks/patterns
                result[key] = self._merge_list_by_id(result[key], value)
            else:
                result[key] = value
        return result

    def _merge_list_by_id(self, base_list: list, override_list: list) -> list:
        """Merge two lists of dicts by their `id` field.

        - Items in override with matching `id` replace the base item
        - Items in override with new `id` are appended
        - Items in base with no matching override are kept
        """
        base_by_id = {item["id"]: item for item in base_list if isinstance(item, dict) and "id" in item}
        seen_ids = set()

        result = []
        for item in base_list:
            item_id = item.get("id") if isinstance(item, dict) else None
            if item_id is not None:
                # Check if override replaces this item
                override_item = next((o for o in override_list if isinstance(o, dict) and o.get("id") == item_id), None)
                if override_item:
                    result.append(override_item)
                    seen_ids.add(item_id)
                else:
                    result.append(item)
                    seen_ids.add(item_id)
            else:
                result.append(item)

        # Append new items from override
        for item in override_list:
            item_id = item.get("id") if isinstance(item, dict) else None
            if item_id is not None and item_id not in seen_ids:
                result.append(item)

        return result

    def clear_cache(self):
        self._cache.clear()
```

### 6.2 loaders/events_loader.py

```python
# loaders/events_loader.py
from .config_loader import ConfigLoader
from pathlib import Path
from typing import Dict, Any, Optional

class EventsLoader(ConfigLoader):
    def __init__(self):
        super().__init__("events.yaml")

    def get_event_config(self, project_path: Path, event_type: str) -> Optional[Dict]:
        config = self.load(project_path)
        return config.get("event_types", {}).get(event_type)

    def get_criticality(self, project_path: Path, event_type: str) -> str:
        event_config = self.get_event_config(project_path, event_type)
        return event_config.get("criticality", "important") if event_config else "important"

    def should_emit_on_error(self, project_path: Path, event_type: str) -> bool:
        event_config = self.get_event_config(project_path, event_type)
        return event_config.get("emit_on_error", False) if event_config else False

_events_loader: Optional[EventsLoader] = None

def get_events_loader() -> EventsLoader:
    global _events_loader
    if _events_loader is None:
        _events_loader = EventsLoader()
    return _events_loader

def load(project_path: Path) -> Dict[str, Any]:
    return get_events_loader().load(project_path)
```

### 6.3 loaders/error_loader.py

```python
# loaders/error_loader.py
from .config_loader import ConfigLoader
from pathlib import Path
from typing import Dict, Any, Optional
import re

class ErrorLoader(ConfigLoader):
    def __init__(self):
        super().__init__("error_classification.yaml")

    def classify(self, project_path: Path, error_context: Dict) -> Dict:
        """Classify an error based on config patterns."""
        config = self.load(project_path)

        for pattern in config.get("patterns", []):
            if self._matches(error_context, pattern.get("match", {})):
                return {
                    "category": pattern.get("category", "permanent"),
                    "retryable": pattern.get("retryable", False),
                    "retry_policy": pattern.get("retry_policy"),
                    "code": pattern.get("id"),
                }

        default = config.get("default", {})
        return {
            "category": default.get("category", "permanent"),
            "retryable": default.get("retryable", False),
        }

    def _matches(self, context: Dict, match: Dict) -> bool:
        """Evaluate match condition against context.

        Uses the same path/op/value + any/all/not combinator format
        as hook conditions.
        """
        if "any" in match:
            return any(self._matches(context, c) for c in match["any"])
        if "all" in match:
            return all(self._matches(context, c) for c in match["all"])
        if "not" in match:
            return not self._matches(context, match["not"])

        path = match.get("path", "")
        op = match.get("op", "eq")
        expected = match.get("value")
        actual = self._resolve_path(context, path)
        return self._apply_operator(actual, op, expected)

    def _resolve_path(self, obj: Any, path: str) -> Any:
        parts = path.split(".")
        current = obj
        for part in parts:
            if isinstance(current, dict):
                current = current.get(part)
            else:
                return None
        return current

    def _apply_operator(self, actual, op: str, expected) -> bool:
        ops = {
            "eq": lambda a, e: a == e,
            "ne": lambda a, e: a != e,
            "gt": lambda a, e: a is not None and a > e,
            "gte": lambda a, e: a is not None and a >= e,
            "lt": lambda a, e: a is not None and a < e,
            "lte": lambda a, e: a is not None and a <= e,
            "in": lambda a, e: a in e if isinstance(e, list) else False,
            "contains": lambda a, e: e in str(a) if a else False,
            "regex": lambda a, e: bool(re.search(e, str(a))) if a else False,
            "exists": lambda a, e: a is not None,
        }
        return ops.get(op, lambda a, e: False)(actual, expected)

    def calculate_retry_delay(self, project_path: Path, policy: Dict, attempt: int) -> float:
        policy_type = policy.get("type", "none")
        if policy_type == "exponential":
            base = policy.get("base", 2.0)
            max_delay = policy.get("max", 120.0)
            return min(base * (2 ** attempt), max_delay)
        if policy_type == "fixed":
            return policy.get("delay", 60.0)
        return 0.0

_error_loader: Optional[ErrorLoader] = None

def get_error_loader() -> ErrorLoader:
    global _error_loader
    if _error_loader is None:
        _error_loader = ErrorLoader()
    return _error_loader

def load(project_path: Path) -> Dict[str, Any]:
    return get_error_loader().load(project_path)

def classify(project_path: Path, error_context: Dict) -> Dict:
    return get_error_loader().classify(project_path, error_context)
```

### 6.4 loaders/hooks_loader.py

```python
# loaders/hooks_loader.py
from .config_loader import ConfigLoader
from pathlib import Path
from typing import Dict, Any, Optional, List

class HooksLoader(ConfigLoader):
    def __init__(self):
        super().__init__("hook_conditions.yaml")

    def get_builtin_hooks(self, project_path: Path) -> List[Dict]:
        config = self.load(project_path)
        return config.get("builtin_hooks", [])

    def get_infra_hooks(self, project_path: Path) -> List[Dict]:
        config = self.load(project_path)
        return config.get("infra_hooks", [])

_hooks_loader: Optional[HooksLoader] = None

def get_hooks_loader() -> HooksLoader:
    global _hooks_loader
    if _hooks_loader is None:
        _hooks_loader = HooksLoader()
    return _hooks_loader

def load(project_path: Path) -> Dict[str, Any]:
    return get_hooks_loader().load(project_path)
```

### 6.5 loaders/resilience_loader.py

```python
# loaders/resilience_loader.py
from .config_loader import ConfigLoader
from pathlib import Path
from typing import Dict, Any, Optional

class ResilienceLoader(ConfigLoader):
    def __init__(self):
        super().__init__("resilience.yaml")

    def get_default_limits(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("limits", {}).get("defaults", {})

    def get_retry_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("retry", {})

    def get_coordination_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("coordination", {})

    def get_child_policy(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("child_policy", {})

_resilience_loader: Optional[ResilienceLoader] = None

def get_resilience_loader() -> ResilienceLoader:
    global _resilience_loader
    if _resilience_loader is None:
        _resilience_loader = ResilienceLoader()
    return _resilience_loader

def load(project_path: Path) -> Dict[str, Any]:
    return get_resilience_loader().load(project_path)
```

### 6.6 events/event_emitter.py

```python
# events/event_emitter.py
from pathlib import Path
from typing import Dict, Any, Optional
import asyncio

class EventEmitter:
    """Emit events to transcript with criticality routing from config."""

    def __init__(self, project_path: Path):
        self.project_path = project_path
        from ..loaders import events_loader
        self.config = events_loader.load(project_path)

    def emit(self, thread_id: str, event_type: str, payload: Dict,
             transcript: Any = None, criticality: Optional[str] = None) -> None:
        if criticality is None:
            event_config = self.config.get("event_types", {}).get(event_type, {})
            criticality = event_config.get("criticality", "important")

        if criticality == "critical":
            self.emit_critical(thread_id, event_type, payload, transcript)
        else:
            self.emit_droppable(thread_id, event_type, payload, transcript)

    def emit_critical(self, thread_id: str, event_type: str, payload: Dict,
                      transcript: Any) -> None:
        if transcript:
            transcript.write_event(thread_id, event_type, payload)

    def emit_droppable(self, thread_id: str, event_type: str, payload: Dict,
                       transcript: Any) -> None:
        if transcript:
            try:
                loop = asyncio.get_event_loop()
                loop.create_task(self._async_emit(transcript, thread_id, event_type, payload))
            except RuntimeError:
                transcript.write_event(thread_id, event_type, payload)

    async def _async_emit(self, transcript: Any, thread_id: str, event_type: str,
                          payload: Dict) -> None:
        try:
            transcript.write_event(thread_id, event_type, payload)
        except Exception:
            pass  # Droppable
```

### 6.7 adapters/tool_dispatcher.py

The `ToolDispatcher` is the translation layer between hook/action dicts and the core tool APIs. Hook actions use `params` but core tools expect `parameters`. The dispatcher handles this mapping and injects `project_path`.

```python
# adapters/tool_dispatcher.py
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import Action
from rye.tools.search import SearchTool
from rye.tools.load import LoadTool
from rye.tools.execute import ExecuteTool
from rye.tools.sign import SignTool
from rye.utils.resolvers import get_user_space

class ToolDispatcher:
    """Dispatch primary tool actions to core RYE tools.

    Translates hook/action dict format to core tool handle() kwargs.

    Action dict format (from hooks and parsed directive actions):
        {"primary": "execute", "item_type": "tool", "item_id": "...", "params": {...}}

    The parser puts XML attributes as top-level keys (e.g., <search query="..." />
    becomes {"primary": "search", "query": "..."}), while <param> children go into
    "params". The dispatcher checks top-level first, then falls back to params.

    Core tool handle() kwargs:
        ExecuteTool.handle(item_type=, item_id=, project_path=, parameters=, dry_run=)
        SearchTool.handle(item_type=, query=, project_path=, source=, limit=)
        LoadTool.handle(item_type=, item_id=, project_path=, source=)
        SignTool.handle(item_type=, item_id=, project_path=, source=)
    """

    def __init__(self, project_path: Path):
        self.project_path = project_path
        user_space = str(get_user_space())
        self._tools = {
            Action.EXECUTE: ExecuteTool(user_space),
            Action.SEARCH: SearchTool(user_space),
            Action.LOAD: LoadTool(user_space),
            Action.SIGN: SignTool(user_space),
        }

    def _get(self, action: Dict, params: Dict, key: str, default: Any = "") -> Any:
        """Resolve a key: top-level action attrs first, then params, then default.

        The parser puts XML attributes as top-level keys on the action dict
        (e.g., <search query="patterns" /> → {"query": "patterns"}).
        Hook configs put values under "params". This method checks both.
        """
        if key in action:
            return action[key]
        return params.get(key, default)

    async def dispatch(self, action: Dict, thread_context: Optional[Dict] = None) -> Dict:
        """Dispatch an action dict to the appropriate core tool.

        Args:
            action: Action dict with primary, item_type, item_id, params
            thread_context: Optional {emitter, transcript, thread_id} for internal tools

        Returns:
            Tool result dict
        """
        primary = action.get("primary", "execute")
        tool = self._tools.get(primary)
        if not tool:
            return {"status": "error", "error": f"Unknown primary action: {primary}"}

        item_type = action.get("item_type", "tool")
        item_id = action.get("item_id", "")
        params = dict(action.get("params", {}))

        # Inject thread context for internal tools
        if thread_context and item_id.startswith("rye/agent/threads/internal/"):
            params["_thread_context"] = thread_context

        project_path_str = str(self.project_path)

        try:
            if primary == Action.EXECUTE:
                return await tool.handle(
                    item_type=item_type,
                    item_id=item_id,
                    project_path=project_path_str,
                    parameters=params,
                    dry_run=action.get("dry_run", False),
                )
            elif primary == Action.SEARCH:
                return await tool.handle(
                    item_type=item_type,
                    query=self._get(action, params, "query"),
                    project_path=project_path_str,
                    source=self._get(action, params, "source", "project"),
                    limit=self._get(action, params, "limit", 10),
                )
            elif primary == Action.LOAD:
                return await tool.handle(
                    item_type=item_type,
                    item_id=item_id,
                    project_path=project_path_str,
                    source=self._get(action, params, "source", "project"),
                )
            elif primary == Action.SIGN:
                return await tool.handle(
                    item_type=item_type,
                    item_id=item_id,
                    project_path=project_path_str,
                    source=self._get(action, params, "source", "project"),
                )
        except Exception as e:
            return {"status": "error", "error": str(e)}

    async def dispatch_parallel(self, actions: list, thread_context: Optional[Dict] = None) -> list:
        """Dispatch multiple actions concurrently."""
        import asyncio
        tasks = [self.dispatch(action, thread_context) for action in actions]
        return await asyncio.gather(*tasks, return_exceptions=True)
```

### 6.8 thread_directive.py (Entry Point)

The thread directive is the entry point — it's a tool that accepts parameters from `rye_execute` and orchestrates a thread execution.

```python
# thread_directive.py
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads"
__tool_description__ = "Execute a directive in a managed thread with LLM loop"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "directive_name": {"type": "string", "description": "Directive item_id to execute"},
        "async_exec": {"type": "boolean", "default": False, "description": "Return immediately with thread_id"},
        "inputs": {"type": "object", "default": {}, "description": "Input parameters for the directive"},
        "parent_thread_id": {"type": "string", "description": "Parent thread for budget/cancel propagation"},
        "model": {"type": "string", "description": "Override LLM model"},
        "limit_overrides": {
            "type": "object",
            "description": "Override default limits (turns, tokens, spend, spawns, duration_seconds)",
        },
    },
    "required": ["directive_name"],
}

# Return schema (sync mode):
SYNC_RETURN = {
    "success": bool,          # True if completed without error
    "thread_id": str,          # Thread ID
    "directive": str,          # Directive name
    "result": Any,             # LLM's final output / structured result
    "cost": {
        "turns": int,          # Number of LLM turns used
        "input_tokens": int,   # Total input tokens
        "output_tokens": int,  # Total output tokens
        "spend": float,        # Total spend in spend_currency
    },
    "status": str,             # "completed" | "error" | "suspended" | "cancelled"
    "error": Optional[str],    # Error message if failed
}

# Return schema (async mode):
ASYNC_RETURN = {
    "success": True,
    "thread_id": str,
    "status": "running",
    "directive": str,
    "control": {
        "wait": "orchestrator.wait_threads(['{thread_id}'])",
        "cancel": "state_store.request_cancel('{thread_id}')",
        "status": "registry.get_status('{thread_id}')",
    },
}
```

**Execution flow:**

```python
import asyncio
import uuid
from pathlib import Path
from typing import Any, Dict

from rye.tools.execute import ExecuteTool
from rye.utils.resolvers import get_user_space

# Sibling imports (within .ai/tools/, not installed packages)
runner = _load_sibling("runner")
SafetyHarness = _load_sibling("safety_harness").SafetyHarness
ToolDispatcher = _load_sibling("adapters/tool_dispatcher").ToolDispatcher
EventEmitter = _load_sibling("events/event_emitter").EventEmitter


def _generate_thread_id() -> str:
    """Generate unique thread ID."""
    return f"thread-{uuid.uuid4().hex[:12]}"


def _resolve_limits(directive_limits: Dict, overrides: Dict, project_path: str) -> Dict:
    """Merge limits: resilience.yaml defaults → directive <limits> → caller overrides."""
    resilience_loader = _load_sibling("loaders/resilience_loader")
    defaults = resilience_loader.load(Path(project_path)).get("defaults", {})
    merged = {**defaults, **directive_limits, **overrides}
    return merged


def _merge_hooks(directive_hooks: list, project_path: str) -> list:
    """Merge hooks from 3 layers: directive (L1) + builtin (L2) + infra (L3).

    Each hook gets a "layer" key (1, 2, or 3). Hooks are sorted by layer.
    """
    hooks_loader = _load_sibling("loaders/hooks_loader")
    builtin = hooks_loader.load_builtin(Path(project_path))    # layer 2
    infra = hooks_loader.load_infra(Path(project_path))        # layer 3

    for h in directive_hooks:
        h.setdefault("layer", 1)
    for h in builtin:
        h.setdefault("layer", 2)
    for h in infra:
        h.setdefault("layer", 3)

    return sorted(directive_hooks + builtin + infra, key=lambda h: h.get("layer", 2))


async def execute(params: Dict, project_path: str) -> Dict:
    thread_id = _generate_thread_id()
    directive_name = params["directive_name"]
    inputs = params.get("inputs", {})

    # 1. Load directive via ExecuteTool (handles parsing, input validation, interpolation)
    #    ExecuteTool._run_directive() now:
    #    - Parses via ParserRouter("markdown_xml")
    #    - Validates required inputs (returns error with declared_inputs if missing)
    #    - Interpolates {input:key}, {input:key?}, {input:key:default} in body, content, actions
    #    - Returns {"data": parsed, "inputs": inputs, ...}
    user_space = str(get_user_space())
    exec_tool = ExecuteTool(user_space=user_space, project_path=project_path)
    result = await exec_tool.handle(
        item_type="directive", item_id=directive_name,
        project_path=project_path, parameters={"inputs": inputs},
    )
    if result["status"] != "success":
        return result  # Missing required inputs, parse error, etc.

    directive = result["data"]
    # directive["body"] and directive["actions"] are already interpolated

    # 2. Resolve limits: resilience.yaml defaults → directive <limits> → caller overrides
    limits = _resolve_limits(directive.get("limits", {}), params.get("limit_overrides", {}), project_path)

    # 3. Merge hooks: directive <hooks> (layer 1) + builtin (layer 2) + infra (layer 3)
    hooks = _merge_hooks(directive.get("hooks", []), project_path)

    # 4. Create safety harness
    proj_path = Path(project_path)
    harness = SafetyHarness(thread_id, limits, hooks, proj_path, directive_name=directive_name)

    # 5. User prompt = the already-interpolated body
    user_prompt = directive.get("body") or directive.get("description", "Execute the directive.")

    # 6. Construct runtime dependencies
    dispatcher = ToolDispatcher(proj_path)
    emitter = EventEmitter(proj_path)
    transcript = None  # TODO: transcript persistence (file or DB backed)

    # 7. Resolve provider adapter
    model = params.get("model") or directive.get("model", {}).get("id") or directive.get("model", {}).get("tier", "general")
    ProviderAdapter = _load_sibling("adapters/provider_adapter").ProviderAdapter
    provider = ProviderAdapter(model=model, provider_config={})  # Config from provider YAML

    # 8. Async mode: spawn and return immediately
    if params.get("async_exec"):
        asyncio.create_task(runner.run(
            thread_id, user_prompt, harness, provider, dispatcher, emitter, transcript, proj_path,
        ))
        return {"success": True, "thread_id": thread_id, "status": "running", "directive": directive_name}

    # 9. Sync mode: run to completion
    result = await runner.run(
        thread_id, user_prompt, harness, provider, dispatcher, emitter, transcript, proj_path,
    )
    return {**result, "directive": directive_name}
```

### 6.9 runner.py (Core LLM Loop)

```python
# runner.py — Core LLM execution loop

import asyncio
from pathlib import Path
from typing import Any, Dict

# Sibling imports
orchestrator = _load_sibling("orchestrator")

async def run(
    thread_id: str,
    user_prompt: str,
    harness: "SafetyHarness",
    provider: "ProviderAdapter",
    dispatcher: "ToolDispatcher",
    emitter: "EventEmitter",
    transcript: Any,
    project_path: Path,
) -> Dict:
    """Execute the LLM loop until completion, error, or limit.

    No system prompt. Tools are passed via API tool definitions.
    Context framing (identity, rules, etc.) injected via thread_started hooks.

    First message construction:
      1. run_hooks_context() dispatches thread_started hooks
      2. Each hook loads a knowledge/tool item, content is extracted
      3. Hook context + directive body assembled into a single user message

    Each turn:
      1. Check limits (pre-turn)
      2. Send messages to LLM via provider
      3. Parse response for tool calls
      4. Execute tool calls via dispatcher
      5. Check limits (post-turn)
      6. Run hooks (after_step)
      7. Check cancellation
    """
    # Thread context: passed to ToolDispatcher for internal tool injection
    thread_ctx = {"emitter": emitter, "transcript": transcript, "thread_id": thread_id}

    # Register with orchestrator for wait/cancel coordination
    orchestrator.register_thread(thread_id, harness)

    messages = []
    cost = {"turns": 0, "input_tokens": 0, "output_tokens": 0, "spend": 0.0}

    try:
        # --- Build first message ---
        # run_hooks_context() dispatches thread_started hooks and collects context.
        # LoadTool results are mapped: result["data"]["content"] → context block.
        # Returns concatenated string (empty if no hooks matched).
        hook_context = await harness.run_hooks_context({
            "directive": harness.directive_name,
            "model": provider.model,
            "limits": harness.limits,
        }, dispatcher)

        # Assemble first user message: hook context (if any) + directive body.
        # This is a single "user" message — NOT a system prompt.
        first_message_parts = []
        if hook_context:
            first_message_parts.append(hook_context)
        first_message_parts.append(user_prompt)
        messages.append({"role": "user", "content": "\n\n".join(first_message_parts)})

        while True:
            # Pre-turn limit check
            limit_result = harness.check_limits(cost)
            if limit_result:
                hook_result = await harness.run_hooks("limit", limit_result, dispatcher, thread_ctx)
                if hook_result:  # Non-None = terminating action
                    return _finalize(thread_id, cost, hook_result, emitter, transcript)

            # Cancellation check
            if harness.is_cancelled():
                return _finalize(thread_id, cost, {"success": False, "status": "cancelled"}, emitter, transcript)

            # LLM call
            cost["turns"] += 1
            emitter.emit(thread_id, "cognition_in", {
                "text": messages[-1]["content"], "role": messages[-1]["role"],
            }, transcript)

            try:
                response = await provider.create_completion(messages, harness.available_tools)
            except Exception as e:
                # Classify error via error_loader
                error_loader = _load_sibling("loaders/error_loader")
                classification = error_loader.classify(project_path, _error_to_context(e))
                hook_result = await harness.run_hooks(
                    "error", {"error": e, "classification": classification}, dispatcher, thread_ctx,
                )
                if hook_result:
                    if hook_result.get("action") == "retry":
                        delay = error_loader.calculate_retry_delay(
                            classification.get("retry_policy", {}), cost["turns"],
                        )
                        await asyncio.sleep(delay)
                        continue
                    return _finalize(thread_id, cost, hook_result, emitter, transcript)
                return _finalize(thread_id, cost, {"success": False, "error": str(e)}, emitter, transcript)

            # Track tokens
            cost["input_tokens"] += response.get("input_tokens", 0)
            cost["output_tokens"] += response.get("output_tokens", 0)
            cost["spend"] += response.get("spend", 0.0)

            emitter.emit(thread_id, "cognition_out", {
                "text": response["text"], "model": provider.model,
            }, transcript)

            # Process tool calls
            tool_calls = response.get("tool_calls", [])
            if not tool_calls:
                # No tool calls = LLM is done
                return _finalize(thread_id, cost, {"success": True, "result": response["text"]}, emitter, transcript)

            for tool_call in tool_calls:
                emitter.emit(thread_id, "tool_call_start", {
                    "tool": tool_call["name"], "call_id": tool_call["id"], "input": tool_call["input"],
                }, transcript)

                result = await dispatcher.dispatch({
                    "primary": "execute",
                    "item_type": "tool",
                    "item_id": tool_call["name"],
                    "params": tool_call["input"],
                }, thread_context=thread_ctx)

                emitter.emit(thread_id, "tool_call_result", {
                    "call_id": tool_call["id"], "output": str(result),
                }, transcript)

                messages.append({"role": "tool", "tool_call_id": tool_call["id"], "content": str(result)})

            # Post-turn hooks
            await harness.run_hooks("after_step", {"cost": cost}, dispatcher, thread_ctx)

    finally:
        # Always complete with orchestrator (enables wait_threads)
        final = {**cost, "status": "completed" if cost.get("turns") else "error"}
        orchestrator.complete_thread(thread_id, final)


def _finalize(thread_id, cost, result, emitter, transcript) -> Dict:
    status = "completed" if result.get("success") else result.get("status", "error")
    emitter.emit(thread_id, f"thread_{status}", {"cost": cost}, transcript, criticality="critical")
    return {**result, "thread_id": thread_id, "cost": cost, "status": status}


def _error_to_context(e: Exception) -> Dict:
    """Convert exception to context dict for error classification."""
    return {
        "error": {
            "type": type(e).__name__,
            "message": str(e),
            "code": getattr(e, "code", None),
        }
    }
```

### 6.10 safety_harness.py

```python
# safety_harness.py — Thread state, limits, and hook evaluation

from pathlib import Path
from typing import Dict, Any, Optional, List

# Sibling imports (thread tools are not installed packages)
condition_evaluator = _load_sibling("loaders/condition_evaluator")
interpolation = _load_sibling("loaders/interpolation")


class SafetyHarness:
    """Manages thread limits, hooks, and cancellation state.

    NOT an execution engine — it checks limits and evaluates hook conditions.
    Hook actions are dispatched through ToolDispatcher by the caller.

    Two hook dispatch methods:
      - run_hooks()         — for error/limit/after_step events. Returns control action or None.
      - run_hooks_context() — for thread_started only. Returns concatenated context string.
    """

    def __init__(
        self,
        thread_id: str,
        limits: Dict,
        hooks: List[Dict],
        project_path: Path,
        directive_name: str = "",
    ):
        self.thread_id = thread_id
        self.limits = limits             # {turns, tokens, spend, ...}
        self.hooks = hooks               # Merged: directive (L1) + builtin (L2) + infra (L3)
        self.project_path = project_path
        self.directive_name = directive_name
        self._cancelled = False
        self.available_tools = []        # Tool schemas for LLM

    # --- Limit Checking ---

    def check_limits(self, cost: Dict) -> Optional[Dict]:
        """Check all limits against current cost. Returns limit event or None."""
        checks = [
            ("turns", cost.get("turns", 0), self.limits.get("turns")),
            ("tokens", cost.get("input_tokens", 0) + cost.get("output_tokens", 0), self.limits.get("tokens")),
            ("spend", cost.get("spend", 0.0), self.limits.get("spend")),
        ]
        for limit_code, current, maximum in checks:
            if maximum is not None and current >= maximum:
                return {
                    "limit_code": f"{limit_code}_exceeded",
                    "current_value": current,
                    "current_max": maximum,
                }
        return None

    # --- Hook Evaluation (Control Flow) ---

    async def run_hooks(
        self, event: str, context: Dict, dispatcher: "ToolDispatcher", thread_context: Dict,
    ) -> Optional[Dict]:
        """Evaluate hooks for error/limit/after_step events.

        Hook evaluation order: layer 1 (directive) → layer 2 (builtin) → layer 3 (infra).
        First hook action that returns a non-None result wins (for control flow).
        Infra hooks (layer 3) always run regardless.

        Args:
            event: Event name (e.g., "error", "limit", "after_step")
            context: Event-specific context dict
            dispatcher: ToolDispatcher for executing hook actions
            thread_context: {emitter, transcript, thread_id} — passed to internal tools

        Returns:
            None = continue, Dict = terminating action (from control.py)
        """
        control_result = None
        for hook in self.hooks:
            if hook.get("event") != event:
                continue
            if not condition_evaluator.matches(context, hook.get("condition", {})):
                continue

            action = hook.get("action", {})
            interpolated = interpolation.interpolate_action(action, context)
            result = await dispatcher.dispatch(interpolated, thread_context=thread_context)

            # Layer 3 (infra) hooks always run but don't control flow
            if hook.get("layer") == 3:
                continue

            # First non-None result from a control tool wins
            if result and result.get("status") != "error" and control_result is None:
                data = result.get("data", result)
                if data is not None and data != {"success": True}:
                    control_result = data

        return control_result

    # --- Hook Evaluation (Context Injection) ---

    async def run_hooks_context(
        self, context: Dict, dispatcher: "ToolDispatcher",
    ) -> str:
        """Run thread_started hooks and collect context blocks.

        Unlike run_hooks(), this method:
        - Only runs hooks with event == "thread_started"
        - Runs ALL matching hooks (no short-circuit)
        - Maps LoadTool results: result["data"]["content"] → context block
        - Returns concatenated context string (empty string if no hooks matched)

        This is the ONLY method that runner.py calls for thread_started.
        """
        context_blocks = []
        for hook in self.hooks:
            if hook.get("event") != "thread_started":
                continue
            if not condition_evaluator.matches(context, hook.get("condition", {})):
                continue

            action = hook.get("action", {})
            interpolated = interpolation.interpolate_action(action, context)
            result = await dispatcher.dispatch(interpolated)

            # Extract content from tool result
            # LoadTool returns: {"status": "success", "data": {"content": "...", ...}}
            # ExecuteTool returns: {"status": "success", "data": {...}}
            if result and result.get("status") == "success":
                data = result.get("data", {})
                content = data.get("content") or data.get("body") or data.get("raw", "")
                if content:
                    context_blocks.append(content.strip())

        return "\n\n".join(context_blocks)

    # --- Cancellation ---

    def request_cancel(self):
        self._cancelled = True

    def is_cancelled(self) -> bool:
        return self._cancelled
```

### 6.11 orchestrator.py

```python
# orchestrator.py — Thread coordination for fan-out/collect pattern
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads"
__tool_description__ = "Thread coordination: wait, cancel, status"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {"type": "string", "enum": ["wait_threads", "cancel_thread", "get_status", "list_active"]},
        "thread_ids": {"type": "array", "items": {"type": "string"}},
        "thread_id": {"type": "string"},
        "timeout": {"type": "number"},
    },
    "required": ["operation"],
}

# --- Module-level state ---
# In-memory dicts for thread coordination. Populated by runner.py.
import asyncio
from pathlib import Path
from typing import Dict, List, Optional

_thread_events: Dict[str, asyncio.Event] = {}
_thread_results: Dict[str, Dict] = {}
_active_harnesses: Dict[str, "SafetyHarness"] = {}  # For cancel dispatch


# --- Public API (called by runner.py) ---

def register_thread(thread_id: str, harness: "SafetyHarness") -> None:
    """Called by runner.py at thread start. Creates Event for wait coordination."""
    _thread_events[thread_id] = asyncio.Event()
    _active_harnesses[thread_id] = harness

def complete_thread(thread_id: str, result: Dict) -> None:
    """Called by runner.py in finally block. Signals Event so wait_threads unblocks."""
    _thread_results[thread_id] = result
    event = _thread_events.get(thread_id)
    if event:
        event.set()
    _active_harnesses.pop(thread_id, None)


# --- Tool entry point (called via rye_execute) ---

async def execute(params: Dict, project_path: str) -> Dict:
    operation = params["operation"]

    if operation == "wait_threads":
        thread_ids = params.get("thread_ids", [])
        timeout = params.get("timeout")  # None = use resilience.yaml default

        if timeout is None:
            resilience_loader = _load_sibling("loaders/resilience_loader")
            config = resilience_loader.load(Path(project_path))
            timeout = config.get("coordination", {}).get("wait_timeout_seconds", 300.0)

        results = {}
        try:
            for tid in thread_ids:
                event = _thread_events.get(tid)
                if event:
                    await asyncio.wait_for(event.wait(), timeout=timeout)
                    results[tid] = _thread_results.get(tid, {"status": "unknown"})
                else:
                    results[tid] = {"status": "not_found"}
        except asyncio.TimeoutError:
            for tid in thread_ids:
                if tid not in results:
                    results[tid] = {"status": "timeout"}

        all_success = all(r.get("success", False) for r in results.values())
        return {"success": all_success, "results": results}

    if operation == "cancel_thread":
        thread_id = params.get("thread_id")
        harness = _active_harnesses.get(thread_id)
        if harness:
            harness.request_cancel()
            return {"success": True, "cancelled": thread_id}
        return {"success": False, "error": f"Thread not found: {thread_id}"}

    if operation == "get_status":
        thread_id = params.get("thread_id")
        if thread_id in _thread_results:
            return {"success": True, **_thread_results[thread_id]}
        if thread_id in _thread_events:
            return {"success": True, "status": "running"}
        return {"success": False, "error": f"Thread not found: {thread_id}"}

    if operation == "list_active":
        active = [tid for tid, event in _thread_events.items() if not event.is_set()]
        return {"success": True, "active_threads": active, "count": len(active)}
```

### 6.12 adapters/provider_adapter.py

```python
# adapters/provider_adapter.py — LLM provider abstraction

class ProviderAdapter:
    """Abstract interface for LLM providers.

    Each provider implementation translates to/from the provider's native API.
    The runner calls only these methods.
    """

    def __init__(self, model: str, provider_config: Dict):
        self.model = model
        self.config = provider_config

    async def create_completion(self, messages: List[Dict], tools: List[Dict]) -> Dict:
        """Send messages to LLM and return structured response.

        Args:
            messages: List of {"role": str, "content": str} message dicts
            tools: List of tool schemas the LLM can call

        Returns:
            {
                "text": str,                    # LLM's text response
                "tool_calls": [                  # Tool calls requested by LLM
                    {
                        "id": str,               # Unique call ID
                        "name": str,             # Tool/item_id to execute
                        "input": Dict,           # Parameters for the tool
                    }
                ],
                "input_tokens": int,             # Tokens consumed by input
                "output_tokens": int,            # Tokens generated
                "spend": float,                  # Cost in spend_currency
                "finish_reason": str,            # "stop" | "tool_calls" | "length"
            }
        """
        raise NotImplementedError

    async def create_streaming_completion(self, messages, tools):
        """Streaming variant — yields chunks."""
        raise NotImplementedError
```

### 6.13 persistence/state_store.py

```python
# persistence/state_store.py — Atomic thread state persistence

STATE_FILE = "thread_state.json"

class StateStore:
    """Persist thread harness state for crash recovery.

    State file location: {project_path}/.ai/threads/{thread_id}/state.json
    Atomic writes via write-to-temp + os.replace().
    """

    def __init__(self, project_path: Path, thread_id: str):
        self.state_dir = project_path / ".ai" / "threads" / thread_id
        self.state_file = self.state_dir / STATE_FILE

    def save(self, state: Dict) -> None:
        """Atomically persist state."""
        self.state_dir.mkdir(parents=True, exist_ok=True)
        tmp = self.state_file.with_suffix(".tmp")
        tmp.write_text(json.dumps(state, default=str))
        os.replace(tmp, self.state_file)

    def load(self) -> Optional[Dict]:
        """Load persisted state, or None if no state file."""
        if self.state_file.exists():
            return json.loads(self.state_file.read_text())
        return None

    def request_cancel(self) -> None:
        """Write cancel sentinel file."""
        cancel_file = self.state_dir / ".cancel"
        cancel_file.touch()

    def is_cancel_requested(self) -> bool:
        return (self.state_dir / ".cancel").exists()
```

### 6.14 persistence/budgets.py

```python
# persistence/budgets.py — SQLite budget ledger

DB_FILE = "budget_ledger.db"

class BudgetLedger:
    """SQLite-backed budget tracking.

    DB location: {project_path}/.ai/threads/budget_ledger.db
    Schema loaded from config/budget_ledger_schema.yaml.
    """

    def __init__(self, project_path: Path):
        self.db_path = project_path / ".ai" / "threads" / DB_FILE
        self._ensure_schema()

    def _ensure_schema(self):
        """Create table if not exists from budget_ledger_schema.yaml."""
        # Load schema config, generate CREATE TABLE, execute
        ...

    def reserve(self, thread_id: str, amount: float, parent_thread_id: str = None) -> bool:
        """Reserve budget. Returns False if parent has insufficient remaining."""
        ...

    def report_actual(self, thread_id: str, amount: float) -> None:
        """Report actual spend (clamped to reserved amount)."""
        ...

    def release(self, thread_id: str) -> None:
        """Release remaining reservation on thread completion/error."""
        ...

    def get_remaining(self, thread_id: str) -> float:
        """Get remaining budget (reserved - actual)."""
        ...

_ledger_cache: Dict[str, BudgetLedger] = {}

def get_ledger(project_path: Path) -> BudgetLedger:
    key = str(project_path)
    if key not in _ledger_cache:
        _ledger_cache[key] = BudgetLedger(project_path)
    return _ledger_cache[key]
```

### 6.15 persistence/thread_registry.py

```python
# persistence/thread_registry.py — Thread lifecycle registry

class ThreadRegistry:
    """Track thread lifecycle in SQLite.

    DB location: {project_path}/.ai/threads/registry.db
    """

    def __init__(self, project_path: Path):
        self.db_path = project_path / ".ai" / "threads" / "registry.db"

    def register(self, thread_id: str, directive: str, parent_id: str = None) -> None: ...
    def update_status(self, thread_id: str, status: str) -> None: ...
    def get_status(self, thread_id: str) -> Optional[str]: ...
    def list_active(self) -> List[Dict]: ...
    def list_children(self, parent_id: str) -> List[Dict]: ...
    def get_thread(self, thread_id: str) -> Optional[Dict]: ...
```

---

## 7. Cross-Cutting Concerns

### 7.1 Interpolation Engine

Hook actions and config values use `${...}` template expressions to reference runtime context. The interpolation engine is simple string substitution — no expressions, no function calls.

**Syntax:**

```
${path.to.value}      — resolve dotted path in context dict
${error.message}      — context["error"]["message"]
${cost.turns}          — context["cost"]["turns"]
${limit_code}          — context["limit_code"] (top-level key)
```

**Rules:**

1. Resolve dotted paths against the context dict (same `_resolve_path` as condition evaluation)
2. Missing paths resolve to empty string `""` (not an error)
3. Non-string values are converted via `str()`
4. Nested `${...}` is not supported
5. Literal `$` is escaped as `$$`

**Implementation:**

```python
import re

_INTERPOLATION_RE = re.compile(r"\$\{([^}]+)\}")

def interpolate(template: Any, context: Dict) -> Any:
    """Interpolate ${...} expressions in a value.

    Works on strings, dicts (recursive), and lists (recursive).
    Non-string leaves are returned as-is.
    """
    if isinstance(template, str):
        def _replace(match):
            path = match.group(1)
            value = _resolve_path(context, path)
            return str(value) if value is not None else ""
        return _INTERPOLATION_RE.sub(_replace, template)
    if isinstance(template, dict):
        return {k: interpolate(v, context) for k, v in template.items()}
    if isinstance(template, list):
        return [interpolate(item, context) for item in template]
    return template

def interpolate_action(action: Dict, context: Dict) -> Dict:
    """Interpolate all ${...} in an action's params. Preserves primary/item_type/item_id.

    Called by SafetyHarness as: interpolation.interpolate_action(action, context)
    """
    result = dict(action)
    if "params" in result:
        result["params"] = interpolate(result["params"], context)
    return result
```

### 7.2 Event Envelope

All events emitted to the transcript share a common envelope:

```python
EVENT_ENVELOPE = {
    "thread_id": str,          # Thread that emitted the event
    "event_type": str,         # e.g., "cognition_out", "tool_call_start"
    "timestamp": str,          # ISO 8601 UTC
    "payload": Dict,           # Event-specific data (schema in events.yaml)
    "criticality": str,        # "critical" | "droppable" (from events.yaml)
    "sequence": int,           # Monotonically increasing per thread
}
```

### 7.3 Shared Condition Evaluation

Both `error_loader._matches()` and `safety_harness._evaluate_condition()` use the same `path`/`op`/`value` + `any`/`all`/`not` combinator format. To avoid duplication, extract to a shared module:

```python
# loaders/condition_evaluator.py
import re
from typing import Any, Dict

def matches(doc: Dict, condition: Dict) -> bool:
    """Evaluate a condition against a document."""
    if not condition:
        return True
    if "any" in condition:
        return any(matches(doc, c) for c in condition["any"])
    if "all" in condition:
        return all(matches(doc, c) for c in condition["all"])
    if "not" in condition:
        return not matches(doc, condition["not"])

    path = condition.get("path", "")
    op = condition.get("op", "eq")
    expected = condition.get("value")
    actual = resolve_path(doc, path)
    return apply_operator(actual, op, expected)

def resolve_path(doc: Dict, path: str) -> Any:
    parts = path.split(".")
    current = doc
    for part in parts:
        if isinstance(current, dict):
            current = current.get(part)
        else:
            return None
    return current

def apply_operator(actual, op: str, expected) -> bool:
    ops = {
        "eq": lambda a, e: a == e,
        "ne": lambda a, e: a != e,
        "gt": lambda a, e: a is not None and a > e,
        "gte": lambda a, e: a is not None and a >= e,
        "lt": lambda a, e: a is not None and a < e,
        "lte": lambda a, e: a is not None and a <= e,
        "in": lambda a, e: a in e if isinstance(e, list) else False,
        "contains": lambda a, e: e in str(a) if a else False,
        "regex": lambda a, e: bool(re.search(e, str(a))) if a else False,
        "exists": lambda a, e: a is not None,
    }
    return ops.get(op, lambda a, e: False)(actual, expected)
```

Both `error_loader` and `safety_harness` import from this module instead of defining their own.

### 7.4 User Prompt Construction

**No system prompt.** The directive's `body` is the task. Tools are passed via API tool definitions. Context framing is injected via `thread_started` hooks (see §7.5).

Input interpolation is handled by `ExecuteTool._run_directive()` (in `rye/rye/tools/execute.py`) **before** the parsed data reaches the thread system. By the time `thread_directive.py` reads `directive["body"]`, all `{input:*}` placeholders are already resolved.

```python
# rye/rye/tools/execute.py — already implemented

# {input:key}          — resolves to value; kept as-is if missing (signals a problem)
# {input:key?}         — resolves to value; empty string if missing
# {input:key:default}  — resolves to value; falls back to default if missing
_INPUT_REF = re.compile(r"\{input:(\w+)(\?|:[^}]*)?\}")

def _resolve_input_refs(value: str, inputs: Dict[str, Any]) -> str:
    """Resolve {input:name} placeholders in a string."""
    def _replace(m: re.Match) -> str:
        key = m.group(1)
        modifier = m.group(2)
        if key in inputs:
            return str(inputs[key])
        if modifier == "?":
            return ""
        if modifier and modifier.startswith(":"):
            return modifier[1:]
        return m.group(0)
    return _INPUT_REF.sub(_replace, value)

def _interpolate_parsed(parsed: Dict[str, Any], inputs: Dict[str, Any]) -> None:
    """Interpolate {input:name} refs in body, actions, and content fields."""
    for key in ("body", "content"):
        if isinstance(parsed.get(key), str):
            parsed[key] = _resolve_input_refs(parsed[key], inputs)
    for action in parsed.get("actions", []):
        for k, v in list(action.items()):
            if isinstance(v, str):
                action[k] = _resolve_input_refs(v, inputs)
        for pk, pv in list(action.get("params", {}).items()):
            if isinstance(pv, str):
                action["params"][pk] = _resolve_input_refs(pv, inputs)
```

`ExecuteTool._run_directive()` also validates required inputs before interpolating:

```python
# Required inputs checked before interpolation
declared_inputs = parsed.get("inputs", [])  # From <input required="true">
missing = [inp["name"] for inp in declared_inputs if inp.get("required") and inp["name"] not in inputs]
if missing:
    return {"status": "error", "error": f"Missing required inputs: {', '.join(missing)}", ...}
```

The thread entry point therefore just reads the already-interpolated body:

```python
# thread_directive.py — no interpolation needed here
user_prompt = directive.get("body") or directive.get("description", "Execute the directive.")
```

### 7.5 Context Injection via Hooks

There is no system prompt. Context framing (identity, rules, project info) is injected via `thread_started` hooks whose results are assembled into the first user message alongside the directive body.

**Hook definition:**

```yaml
# hook_conditions.yaml — Layer 2 (builtin) or project override
builtin_hooks:
  - id: "inject_identity"
    event: "thread_started"
    layer: 2
    condition: {} # always runs
    action:
      primary: "load"
      item_type: "knowledge"
      item_id: "rye/core/identity"
      params:
        source: "system" # Identity lives in system space, not project
      # LoadTool returns: {"status": "success", "data": {"content": "You are Rye..."}}
      # run_hooks_context() extracts data["content"] → context block

  - id: "inject_project_rules"
    event: "thread_started"
    layer: 2
    condition:
      path: "directive"
      op: "ne"
      value: "" # only when running a named directive
    action:
      primary: "load"
      item_type: "knowledge"
      item_id: "project/rules"
      # source defaults to "project" — correct for project-level knowledge
```

**How it flows into the first message:**

Runner calls `harness.run_hooks_context()` (not `run_hooks()`) for thread_started. This method:

1. Finds all hooks with `event: "thread_started"` whose conditions match
2. Dispatches each hook action via `ToolDispatcher` (e.g., `LoadTool.handle()`)
3. Extracts content from each result: `result["data"]["content"]` or `result["data"]["body"]`
4. Returns all blocks concatenated as a single string

```python
# runner.py — first message assembly (see §6.9)
hook_context = await harness.run_hooks_context({
    "directive": harness.directive_name,
    "model": provider.model,
    "limits": harness.limits,
}, dispatcher)

# hook_context is a string (e.g., "You are Rye...\n\nFollow project rules: ...")
first_message_parts = []
if hook_context:
    first_message_parts.append(hook_context)
first_message_parts.append(user_prompt)                     # directive body

# Single user message — LLM sees context framing above the task
messages.append({"role": "user", "content": "\n\n".join(first_message_parts)})
```

**What the LLM sees (single user message):**

```
You are Rye, an execution agent. Use tools via the tool_use API.

Follow project rules: ...

Research rust and write findings to /tmp/research.md.
```

**Two hook dispatch methods (see §6.10 SafetyHarness):**

| Method                | Used for                       | Returns                             | Short-circuits?                  |
| --------------------- | ------------------------------ | ----------------------------------- | -------------------------------- |
| `run_hooks_context()` | `thread_started` only          | `str` (concatenated content blocks) | No — runs ALL matching hooks     |
| `run_hooks()`         | `error`, `limit`, `after_step` | `Optional[Dict]` (control action)   | Yes — first non-None result wins |

**Hook evaluation order:** layer 1 (directive) → layer 2 (builtin) → layer 3 (infra). Hooks are sorted by layer in `_merge_hooks()` (see §6.8). Layer 3 (infra) hooks always run in `run_hooks()` but don't control flow.

This allows:

- **Identity**: Load a knowledge entry with agent persona from system space
- **Rules**: Load project-specific constraints from project space
- **Conditional injection**: Use `condition` to inject only for certain directives or models
- **No format mismatch**: `run_hooks_context()` maps LoadTool's `data.content` → context string internally

### 7.6 Core Tool Result Contract

All 4 primary tools return a dict with at minimum `status`:

```python
# Success
{"status": "success", "type": "...", "item_id": "...", "data": {...}, ...}

# Error
{"status": "error", "error": "error message", "item_id": "...", ...}
```

**Per-tool `data` shapes (used by `run_hooks_context()` to extract content):**

| Tool                      | `data` shape                                                                     | Content extraction                                   |
| ------------------------- | -------------------------------------------------------------------------------- | ---------------------------------------------------- |
| `LoadTool`                | `{"content": str, "title": str, ...}` (knowledge) or `{"raw": str, ...}` (tools) | `data["content"]` or `data["body"]` or `data["raw"]` |
| `ExecuteTool` (directive) | `{"body": str, "actions": [...], ...}`                                           | `data["body"]`                                       |
| `ExecuteTool` (tool)      | Tool-specific result                                                             | `data` as-is                                         |
| `SearchTool`              | `{"results": [...]}`                                                             | Not used for context injection                       |
| `SignTool`                | `{"signed": True, ...}`                                                          | Not used for context injection                       |

### 7.7 Config Caching

Loaders cache configs per project:

```python
from loaders import events_loader, error_loader, hooks_loader, resilience_loader

events_loader.get_events_loader().clear_cache()
error_loader.get_error_loader().clear_cache()
hooks_loader.get_hooks_loader().clear_cache()
resilience_loader.get_resilience_loader().clear_cache()
```

---

## 8. Testing Strategy

| Module                   | Key Test Cases                                                                           |
| ------------------------ | ---------------------------------------------------------------------------------------- |
| `condition_evaluator`    | All 10 operators, `any`/`all`/`not` combinators, dotted path resolution, missing paths   |
| `interpolation`          | `${path.to.value}` resolution, missing paths → `""`, nested dicts/lists, `$$` escaping   |
| `config_loader`          | Load YAML, deep merge dicts, merge-by-id for lists, project override, `extends` skip     |
| `events_loader`          | Get event config, criticality lookup, emit_on_error                                      |
| `error_loader`           | Pattern matching via `condition_evaluator`, retry delay calculation                      |
| `hooks_loader`           | Load builtin hooks, load infra hooks, project override merge-by-id                       |
| `resilience_loader`      | Get defaults (`turns`/`tokens` not `max_*`), retry config, child policy                  |
| `tool_dispatcher`        | Dispatch all 4 primaries (execute/search/load/sign), `params`→`parameters` translation   |
| `tool_dispatcher`        | `project_path` injection, `_thread_context` injection for internal tools                 |
| `internal/control`       | All 7 action types return correct result shape                                           |
| `internal/emitter`       | Emit with context injection, missing context returns error                               |
| `internal/classifier`    | Delegates to `error_loader.classify()`                                                   |
| `internal/limit_checker` | Check each limit type, on_exceed behavior from config                                    |
| `internal/budget_ops`    | Reserve/report/release/check_remaining operations                                        |
| `event_emitter`          | Criticality routing from config, sync vs async, droppable failures silenced              |
| `safety_harness`         | Limit checking (turns/tokens/spend), hook evaluation + dispatch, cancellation            |
| `safety_harness`         | Layer 3 hooks always run, layer 1-2 first non-None wins, interpolation in action params  |
| `runner`                 | Full turn cycle, error classification + retry, tool call dispatch, stop on no tool calls |
| `runner`                 | Limit exceeded → hook → escalate/fail, cancellation mid-loop                             |
| `orchestrator`           | wait_threads with timeout, cancel_thread, get_status, list_active                        |
| `thread_directive`       | Sync mode end-to-end, async mode returns immediately, limit_overrides applied            |
| `thread_directive`       | Uses `ParserRouter("markdown_xml")`, reads `actions` not `steps`                         |
| `state_store`            | Atomic save/load, cancel sentinel file, missing state returns None                       |
| `provider_adapter`       | Response schema validation, token/spend tracking                                         |
| `user prompt`            | `body` already interpolated by `ExecuteTool`, fallback to description                    |
| `input interpolation`    | `{input:key}`, `{input:key?}`, `{input:key:default}` — tested in `test_execute.py`       |
| `input validation`       | Required inputs checked before interpolation, returns error with `declared_inputs`       |
| `context injection`      | `thread_started` hooks return `{"context": ...}`, assembled into single first message    |

---

## 9. Migration Checklist

### Phase 0: Core Prerequisites

- [ ] **Extend `rye/rye/utils/validators.py`** — add `integer`, `number`, and `boolean` type validation to `validate_field()`. Without this, `limits` fields in `directive_extractor.yaml` are not type-checked.
- [ ] Verify `rye` is importable in `${RYE_PYTHON}` venv (thread tools import `from rye.constants import Action`, etc.)

### Phase 1: Shared Utilities

- [ ] Create `loaders/condition_evaluator.py` — shared `matches()`, `resolve_path()`, `apply_operator()`
- [ ] Create `loaders/interpolation.py` — `interpolate()`, `interpolate_action()` with `${...}` syntax

### Phase 2: Config Files

- [ ] Create `config/events.yaml` from data-driven-thread-events.md
- [ ] Create `config/error_classification.yaml` from data-driven-error-classification.md
- [ ] Create `config/hook_conditions.yaml` from data-driven-hooks.md
- [ ] Create `config/resilience.yaml` — use `tokens` not `max_tokens`
- [ ] Create `config/budget_ledger_schema.yaml` from data-driven-budget-ledger.md

### Phase 3: Loaders

- [ ] Create `loaders/config_loader.py` — base YAML loader with extends + merge-by-id
- [ ] Create `loaders/events_loader.py`
- [ ] Create `loaders/error_loader.py` — uses `condition_evaluator.matches()`
- [ ] Create `loaders/hooks_loader.py`
- [ ] Create `loaders/resilience_loader.py`

### Phase 4: Persistence & Infrastructure

Persistence modules must exist before internal tools that depend on them (e.g., `budget_ops` → `budgets.py`, `state_persister` → `state_store.py`).

- [ ] Create `persistence/state_store.py` — atomic JSON + cancel sentinel at `.ai/threads/{id}/`
- [ ] Create `persistence/budgets.py` — SQLite at `.ai/threads/budget_ledger.db`
- [ ] Create `persistence/thread_registry.py` — SQLite at `.ai/threads/registry.db`
- [ ] Create `events/event_emitter.py` — use events_loader for criticality
- [ ] Create `events/streaming_tool_parser.py` — parse streaming chunks
- [ ] Create `security/security.py` — capability tokens, redaction

### Phase 5: Adapters & Internal Tools

- [ ] Create `adapters/tool_dispatcher.py` — translates action dicts to core tool `handle()` kwargs, uses `_get()` for top-level + params fallback
- [ ] Create `adapters/provider_adapter.py` — abstract LLM provider interface with response schema
- [ ] Create `internal/control.py` — standard tool contract (`__version__`, `__category__`, etc.)
- [ ] Create `internal/emitter.py` — emit events with context injection
- [ ] Create `internal/classifier.py` — thin wrapper over `error_loader.classify()`
- [ ] Create `internal/limit_checker.py` — check limits via `resilience_loader`
- [ ] Create `internal/budget_ops.py` — budget operations (imports `persistence/budgets.py`)
- [ ] Create `internal/cost_tracker.py` — track LLM costs
- [ ] Create `internal/state_persister.py` — persist state (imports `persistence/state_store.py`)
- [ ] Create `internal/cancel_checker.py` — check cancellation (imports `persistence/state_store.py`)
- [ ] Sign all internal tools via `rye_sign`

### Phase 6: Core Execution

- [ ] Create `safety_harness.py` — limits + hook evaluation via `condition_evaluator` + dispatch via `ToolDispatcher`
- [ ] Create `runner.py` — LLM loop: provider call → tool dispatch → hook eval → limit check
- [ ] Create `orchestrator.py` — in-memory `asyncio.Event` coordination, wait/cancel/status

### Phase 7: Entry Point

- [ ] Create `thread_directive.py` — uses `ParserRouter("markdown_xml")`, `ToolDispatcher`, `SafetyHarness`

### Verification

- [ ] All configs load correctly
- [ ] Project overrides work (dict deep-merge + list merge-by-id)
- [ ] `ToolDispatcher._get()` resolves top-level action attrs before falling back to `params`
- [ ] `ToolDispatcher` correctly translates `params`→`parameters` for all 4 primaries
- [ ] Hook actions work on all 3 item types (directive/tool/knowledge)
- [ ] `${...}` interpolation resolves paths, handles missing gracefully
- [ ] `condition_evaluator` shared between error_loader and safety_harness
- [ ] Error classification uses config patterns via `error_loader.classify()`
- [ ] Event emitter uses envelope with thread_id, timestamp, sequence
- [ ] All limit fields use canonical names (`turns`, `tokens`, not `max_*`)
- [ ] `SafetyHarness` has two hook methods: `run_hooks()` (control) and `run_hooks_context()` (thread_started)
- [ ] `run_hooks_context()` maps LoadTool `data.content` → context string
- [ ] `run_hooks()` receives real `thread_context` (emitter, transcript, thread_id) — not None
- [ ] `orchestrator.register_thread()` called by runner at start, `complete_thread()` in finally
- [ ] `_active_harnesses` dict defined in orchestrator module scope
- [ ] Context injection hooks specify `source: "system"` for system-space knowledge items
- [ ] Hook evaluation order: layer 1 → 2 → 3 (sorted by `_merge_hooks()` in thread_directive)
- [x] `directive.get("actions")` used everywhere, never `directive.get("steps")`
- [x] `body` used as user prompt (already interpolated by `ExecuteTool`), `content` as reference XML
- [x] `{input:key}`, `{input:key?}`, `{input:key:default}` interpolation implemented in `execute.py`
- [x] Required input validation in `ExecuteTool._run_directive()` before interpolation
- [x] Parser scans entire XML tree for action tags, not just `<process>/<step>`
- [x] All action tags use uniform `action.update(elem.attrib)` — no `execute` special-casing
- [ ] Sibling imports use `importlib.util` with sanitized module names
- [ ] Package imports use `from rye.constants import ...` (requires `rye` in venv)
- [ ] Internal tools follow standard tool contract (signable, discoverable)
- [ ] Persistence paths: `.ai/threads/{thread_id}/state.json`, `.ai/threads/budget_ledger.db`, `.ai/threads/registry.db`

---

## Appendix: Original Docs Reference

The YAML schemas in this plan are derived from:

- `new_docs/concepts/data-driven-thread-events.md` → `config/events.yaml`
- `new_docs/concepts/data-driven-error-classification.md` → `config/error_classification.yaml`
- `new_docs/concepts/data-driven-hooks.md` → `config/hook_conditions.yaml`
- `new_docs/concepts/data-driven-resilience-config.md` → `config/resilience.yaml`
- `new_docs/concepts/data-driven-budget-ledger.md` → `config/budget_ledger_schema.yaml`
- `new_docs/concepts/data-driven-coordination-config.md` → merged into `resilience.yaml`
- `new_docs/concepts/data-driven-state-persistence.md` → handled by `internal/state_persister.py`
- `new_docs/concepts/data-driven-streaming-config.md` → handled by `streaming_tool_parser.py`

## Appendix: Core Components Reference

These already exist and the thread system uses them — do not reimplement:

| Component                  | Path                                       | What It Does                                                                                              |
| -------------------------- | ------------------------------------------ | --------------------------------------------------------------------------------------------------------- |
| `constants.py`             | `rye/rye/constants.py`                     | `Action.ALL = ["search", "sign", "load", "execute"]`, `ItemType.ALL = ["directive", "tool", "knowledge"]` |
| `server.py`                | `rye/rye/server.py`                        | MCP server exposing 4 tools for 3 item types                                                              |
| `SearchTool`               | `rye/rye/tools/search.py`                  | BM25-scored search, field weights from extractors                                                         |
| `LoadTool`                 | `rye/rye/tools/load.py`                    | Load + copy between spaces, integrity verification                                                        |
| `ExecuteTool`              | `rye/rye/tools/execute.py`                 | Execute via `PrimitiveExecutor` chain resolution                                                          |
| `SignTool`                 | `rye/rye/tools/sign.py`                    | Schema-driven validation + signing, routes by extension                                                   |
| `PrimitiveExecutor`        | `rye/rye/executor/`                        | Chain: tool → runtime → primitive, ENV_CONFIG resolution                                                  |
| `ParserRouter`             | `rye/rye/utils/parser_router.py`           | Routes to `markdown_xml`, `python_ast`, `yaml`, etc.                                                      |
| `validators`               | `rye/rye/utils/validators.py`              | `validate_parsed_data()`, `apply_field_mapping()` from extractors                                         |
| `extensions`               | `rye/rye/utils/extensions.py`              | `get_tool_extensions()` from `tool_extractor.yaml`                                                        |
| `signature_formats`        | `rye/rye/utils/signature_formats.py`       | Format from `*_extractor.yaml`                                                                            |
| `directive_extractor.yaml` | `.ai/tools/rye/core/extractors/directive/` | Parser: `markdown_xml`, extracts: actions, limits, hooks, body, content                                   |
| `tool_extractor.yaml`      | `.ai/tools/rye/core/extractors/tool/`      | Parser: `python_ast`, extensions: `.py .yaml .yml .json .js .sh .toml`                                    |
| `knowledge_extractor.yaml` | `.ai/tools/rye/core/extractors/knowledge/` | Parser: `markdown_frontmatter`                                                                            |
| `python_runtime.yaml`      | `.ai/tools/rye/core/runtimes/`             | Runtime for `.py` tools → `subprocess.yaml`                                                               |
| `subprocess.yaml`          | `.ai/tools/rye/core/primitives/`           | Root primitive for shell execution                                                                        |
