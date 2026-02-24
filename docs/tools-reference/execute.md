```yaml
id: execute
title: "rye_execute"
description: Execute directives, tools, or knowledge items
category: tools-reference
tags: [execute, mcp-tool, api]
version: "1.0.0"
```

# rye_execute

Execute directives, tools, or knowledge items. Routes execution based on `item_type`: directives are validated and executed in a managed thread, tools are executed via the PrimitiveExecutor chain, and knowledge entries are parsed and returned as context.

## Parameters

| Parameter      | Type   | Required | Default | Description                                                                              |
| -------------- | ------ | -------- | ------- | ---------------------------------------------------------------------------------------- |
| `item_type`    | string | yes      | —       | `"directive"`, `"tool"`, or `"knowledge"`                                                |
| `item_id`      | string | yes      | —       | Relative path from `.ai/<type>/` without extension (e.g., `"rye/core/create_directive"`) |
| `project_path` | string | yes      | —       | Absolute path to the project root                                                        |
| `parameters`   | dict   | no       | `{}`    | Parameters to pass to the item                                                           |
| `dry_run`      | bool   | no       | `false` | Validate without executing                                                               |
| `async`        | bool   | no       | `false` | For directives: return immediately with `thread_id` instead of waiting for completion     |
| `model`        | string | no       | —       | For directives: override LLM model for thread execution                                  |
| `limit_overrides` | object | no    | —       | For directives: override limits (`turns`, `tokens`, `spend`, `spawns`, `duration_seconds`, `depth`) |

## Item Resolution

Items are resolved by searching three spaces in order: **project** → **user** → **system**.

```
project:  <project_path>/.ai/{item_type}/<item_id>.py
user:     <USER_SPACE>/.ai/{item_type}/<item_id>.py
system:   <rye-package>/.ai/{item_type}/<item_id>.py
```

File extensions are tried automatically based on item type. Directives and knowledge use `.md`. Tools try `.py`, `.yaml`, `.yml`, `.js`, `.sh`, and others registered via extractors.

## Behavior by Item Type

### Directives

Validates inputs, then spawns a managed thread to execute the directive. The thread runs with its own LLM loop, safety harness, and budgets.

**Input validation:**

1. Declared inputs with `default` values are applied first
2. Required inputs without values produce an error with the full `declared_inputs` list
3. Placeholders are interpolated in `body`, `content`, `raw`, and all `actions`

**Input interpolation syntax:**

| Syntax                | Behavior                                    |
| --------------------- | ------------------------------------------- |
| `{input:key}`         | Required — kept as-is if missing            |
| `{input:key?}`        | Optional — replaced with empty string       |
| `{input:key:default}` | Fallback — uses `default` if key is missing |
| `{input:key\|default}` | Fallback — uses `default` if key is missing (pipe syntax) |

**Execution flow:**

1. Parse and validate inputs (same validation as above)
2. Spawn a managed thread via `thread_directive` infrastructure
3. Block until the thread completes and return thread metadata

If `async: true`, returns immediately with `thread_id` and `pid` instead of blocking until completion.

**Response fields:**

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

**Async response:**

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

**Dry run:** Returns `"status": "validation_passed"` after parsing and input validation, without spawning a thread.

**`<returns>` injection:** When a directive is executed, the infrastructure transforms the directive's `<outputs>` into a `<returns>` block appended to the end of the rendered prompt. This tells the LLM what structured output keys to produce. The LLM never sees the raw `<outputs>` XML — it sees the deterministically generated `<returns>` section after the process steps. See [Authoring Directives — How Outputs Become `<returns>`](../authoring/directives.md#how-outputs-become-returns-in-the-prompt) for details.

### Tools

Executes through the PrimitiveExecutor with recursive chain resolution:

1. **Build chain** — Resolves the tool's `__executor_id__` to find the runtime, then the runtime's executor to find the primitive. Produces a chain like: `tool → runtime → primitive`.
2. **Validate chain** — Checks space compatibility and I/O matching between chain elements.
3. **Resolve ENV_CONFIG** — Environment variables and secrets are resolved through the chain.
4. **Execute** — The root Lilux primitive (e.g., `subprocess`, `http_client`) runs the tool.

**Response fields:**

```json
{
  "status": "success",
  "type": "tool",
  "item_id": "rye/file-system/write",
  "data": { "...execution output..." },
  "chain": ["rye/file-system/write", "rye/core/runtimes/python/script", "rye/core/primitives/subprocess"],
  "metadata": { "duration_ms": 45 }
}
```

**Dry run:** Builds and validates the executor chain without executing. Returns chain details and validated pairs on success, or specific chain validation errors on failure.

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

Parses markdown with YAML frontmatter and returns the content for the agent to use as context.

**Response fields:**

```json
{
  "status": "success",
  "type": "knowledge",
  "item_id": "rye/core/directive-metadata-reference",
  "data": { "...parsed frontmatter + content..." },
  "instructions": "Use this knowledge to inform your decisions.",
  "metadata": { "duration_ms": 3 }
}
```

## Integrity Verification

All items are verified against their signature before execution. If an item has been modified since signing, or moved without re-signing, execution fails with an `IntegrityError`.

## Error Responses

Errors return `"status": "error"` with an `error` message and `item_id`:

```json
{
  "status": "error",
  "error": "Missing required inputs: name, category",
  "item_id": "rye/core/create_directive",
  "declared_inputs": [
    { "name": "name", "type": "string", "required": true },
    { "name": "category", "type": "string", "required": true },
    {
      "name": "description",
      "type": "string",
      "required": false,
      "default": ""
    }
  ]
}
```

Tool chain failures include the partial chain and metadata:

```json
{
  "status": "error",
  "error": "Chain validation failed: incompatible spaces",
  "item_id": "rye/file-system/write",
  "chain": ["rye/file-system/write"],
  "metadata": { "duration_ms": 8 }
}
```

## Examples

### Execute a directive with inputs

```python
# Spawns a managed thread and blocks until completion
rye_execute(
    item_type="directive",
    item_id="rye/core/create_directive",
    project_path="/home/user/my-project",
    parameters={
        "name": "deploy_app",
        "category": "workflows",
        "description": "Deploy the application to production"
    }
)
```

### Execute a directive asynchronously

```python
# Returns immediately with thread_id
rye_execute(
    item_type="directive",
    item_id="my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin", "batch_size": 5},
    limit_overrides={"turns": 30, "spend": 3.00},
    async=True
)
```

### Execute a tool

```python
rye_execute(
    item_type="tool",
    item_id="rye/file-system/write",
    project_path="/home/user/my-project",
    parameters={
        "path": "output.txt",
        "content": "hello world"
    }
)
```

### Dry-run a tool to validate its chain

```python
rye_execute(
    item_type="tool",
    item_id="rye/bash/bash",
    project_path="/home/user/my-project",
    parameters={"command": "echo test"},
    dry_run=True
)
```

### Load a knowledge entry

```python
rye_execute(
    item_type="knowledge",
    item_id="rye/core/directive-metadata-reference",
    project_path="/home/user/my-project"
)
```


