<!-- rye:signed:2026-02-26T06:42:50Z:055b049709664ff5c85c8d5bc20c6b8ff00615529fdd78af4c436c3176f386b9:c67apy3VwDuCGNPs3uXUq8q2kvZJDjiD3YC_PtII6HvrXSpaeBjMipQenW8vTgbKH3sqzuuAAqn-ymsEUfinAg==:4b987fd4e40303ac -->
```yaml
name: execute-semantics
title: "rye_execute — MCP Tool Semantics"
entry_type: reference
category: rye/primary
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - execute
  - mcp-tool
  - api
references:
  - search-semantics
  - load-semantics
  - sign-semantics
  - "docs/tools-reference/execute.md"
```

# rye_execute — MCP Tool Semantics

Execute directives, tools, or knowledge items. Routes execution by `item_type`.

## Parameters

| Parameter      | Type   | Required | Default | Description                                                              |
| -------------- | ------ | -------- | ------- | ------------------------------------------------------------------------ |
| `item_type`    | string | yes      | —       | `"directive"`, `"tool"`, or `"knowledge"`                                |
| `item_id`      | string | yes      | —       | Relative path from `.ai/<type>/` without extension                       |
| `project_path` | string | yes      | —       | Absolute path to the project root                                        |
| `parameters`   | dict   | no       | `{}`    | Parameters to pass to the item                                           |
| `dry_run`      | bool   | no       | `false` | Validate without executing                                               |
| `thread`       | bool   | no       | `false` | For directives: spawn a managed thread instead of returning content in-thread |
| `async`        | bool   | no       | `false` | For directives (requires `thread=true`): return immediately with `thread_id` instead of waiting |
| `model`        | string | no       | —       | For directives (requires `thread=true`): override LLM model for thread execution |
| `limit_overrides` | object | no    | —       | For directives (requires `thread=true`): override limits (`turns`, `tokens`, `spend`, `spawns`, `duration_seconds`, `depth`) |

## Item Resolution Order

Items are resolved across three spaces in priority order:

```
project:  <project_path>/.ai/{item_type}/<item_id>.<ext>
user:     <USER_SPACE>/.ai/{item_type}/<item_id>.<ext>
system:   <rye-package>/.ai/{item_type}/<item_id>.<ext>
```

Extensions tried automatically:
- **Directives / Knowledge:** `.md`
- **Tools:** `.py`, `.yaml`, `.yml`, `.js`, `.sh`, and others via extractors

## Integrity Verification

All items are verified against their signature before execution. Modified or moved files without re-signing produce an `IntegrityError`.

## Routing by Item Type

### Directives

Two execution modes controlled by the `thread` parameter:

#### In-thread mode (default, `thread=false`)

1. Parse markdown+XML directive file
2. Validate required inputs; apply defaults
3. Interpolate `{input:name}` placeholders in body, content, raw, and all actions
4. Return parsed directive content with `your_directions` field

The calling agent follows the directive steps in its own context. No LLM infrastructure required.

#### Threaded mode (`thread=true`)

1. Parse markdown+XML directive file
2. Validate required inputs; apply defaults
3. Interpolate `{input:name}` placeholders in body, content, raw, and all actions
4. Spawn a managed thread to execute the directive (LLM loop, safety harness, budgets)
5. Block until thread completes and return thread metadata

If `async: true`, returns immediately with `thread_id` and `pid` instead of blocking.

**Input interpolation syntax:**

| Syntax                | Behavior                              |
| --------------------- | ------------------------------------- |
| `{input:key}`         | Required — kept as-is if missing      |
| `{input:key?}`        | Optional — replaced with empty string |
| `{input:key:default}` | Fallback — uses `default` if missing  |
| `{input:key\|default}` | Fallback — uses `default` if missing (pipe syntax) |

**Input validation order:**
1. Declared inputs with `default` values applied first
2. Required inputs without values → error with `declared_inputs` list
3. Placeholders interpolated in body, content, raw, and all actions

**In-thread response (default):**

```json
{
  "status": "success",
  "type": "directive",
  "item_id": "rye/core/create_directive",
  "your_directions": "<DIRECTIVE_INSTRUCTION constant>",
  "body": "<interpolated directive body>",
  "outputs": [{ "name": "result", "type": "string" }]
}
```

**Threaded response (`thread=true`):**

```json
{
  "status": "success",
  "type": "directive",
  "item_id": "my-project/run_pipeline",
  "thread_id": "my-project/run_pipeline/run_pipeline-1739820456",
  "directive": "my-project/run_pipeline",
  "metadata": { "duration_ms": 45200 }
}
```

**Threaded async response (`thread=true, async=true`):**

```json
{
  "status": "success",
  "type": "directive",
  "item_id": "my-project/run_pipeline",
  "thread_id": "my-project/run_pipeline/run_pipeline-1739820456",
  "directive": "my-project/run_pipeline",
  "status": "running",
  "pid": 42857
}
```

**Dry run:** Returns `"status": "validation_passed"` after parsing and input validation, without executing or spawning a thread.

### Tools

Executes through PrimitiveExecutor with recursive chain resolution:

1. **Build chain** — Resolve `__executor_id__` → runtime → primitive. Produces chain: `tool → runtime → primitive`
2. **Validate chain** — Check space compatibility and I/O matching between chain elements
3. **Resolve ENV_CONFIG** — Environment variables and secrets resolved through chain
4. **Execute** — Root Lilux primitive (`subprocess`, `http_client`, etc.) runs the tool

**Response:**

```json
{
  "status": "success",
  "type": "tool",
  "item_id": "rye/file-system/write",
  "data": { "...execution output..." },
  "chain": [
    "rye/file-system/write",
    "rye/core/runtimes/python/script",
    "rye/core/primitives/subprocess"
  ],
  "metadata": { "duration_ms": 45 }
}
```

**Dry run:** Builds and validates chain without executing. Returns chain details and validated pairs:

```json
{
  "status": "validation_passed",
  "message": "Tool chain validation passed (dry run)",
  "item_id": "rye/file-system/write",
  "chain": ["..."],
  "validated_pairs": ["..."]
}
```

### Knowledge

Parses markdown with YAML frontmatter and returns content as agent context.

**Response:**

```json
{
  "status": "success",
  "type": "knowledge",
  "item_id": "rye/core/directive-metadata-reference",
  "data": { "...parsed frontmatter + content..." },
  "your_directions": "Use this knowledge to inform your decisions.",
  "metadata": { "duration_ms": 3 }
}
```

## `<returns>` Injection

When a directive is executed, the infrastructure transforms the directive's `<outputs>` into a `<returns>` block appended to the rendered prompt. The LLM never sees raw `<outputs>` XML — it sees the deterministically generated `<returns>` section after the process steps, specifying what structured output keys to produce.

## Error Response Format

All errors return `"status": "error"`:

```json
{
  "status": "error",
  "error": "Missing required inputs: name, category",
  "item_id": "rye/core/create_directive",
  "declared_inputs": [
    { "name": "name", "type": "string", "required": true },
    { "name": "category", "type": "string", "required": true },
    { "name": "description", "type": "string", "required": false, "default": "" }
  ]
}
```

Tool chain failures include partial chain and metadata:

```json
{
  "status": "error",
  "error": "Chain validation failed: incompatible spaces",
  "item_id": "rye/file-system/write",
  "chain": ["rye/file-system/write"],
  "metadata": { "duration_ms": 8 }
}
```

## Dry Run Summary

| Item Type   | Dry Run Behavior                                          |
| ----------- | --------------------------------------------------------- |
| `directive` | Parse + validate inputs → `"validation_passed"`           |
| `tool`      | Build + validate chain → `"validation_passed"` with chain |
| `knowledge` | N/A (knowledge execute is always read-only)               |

## Usage Examples

```python
# Directive with inputs (default: returns content in-thread)
rye_execute(
    item_type="directive",
    item_id="rye/core/create_directive",
    project_path="/home/user/my-project",
    parameters={"name": "deploy_app", "category": "workflows"}
)

# Directive in a managed thread
rye_execute(
    item_type="directive",
    item_id="my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin", "batch_size": 5},
    thread=True
)

# Async directive in a managed thread
rye_execute(
    item_type="directive",
    item_id="my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin", "batch_size": 5},
    thread=True,
    limit_overrides={"turns": 30, "spend": 3.00},
    async=True
)

# Tool execution
rye_execute(
    item_type="tool",
    item_id="rye/file-system/write",
    project_path="/home/user/my-project",
    parameters={"path": "output.txt", "content": "hello world"}
)

# Dry-run tool chain validation
rye_execute(
    item_type="tool",
    item_id="rye/bash/bash",
    project_path="/home/user/my-project",
    parameters={"command": "echo test"},
    dry_run=True
)

# Knowledge entry
rye_execute(
    item_type="knowledge",
    item_id="rye/core/directive-metadata-reference",
    project_path="/home/user/my-project"
)
```
