```yaml
id: execute
title: "rye_execute"
description: Execute directives, tools, or knowledge items
category: tools-reference
tags: [execute, mcp-tool, api]
version: "1.0.0"
```

# rye_execute

Execute directives, tools, or knowledge items. Routes execution based on `item_type`: directives are validated and returned in-thread by default (set `thread="fork"` to spawn a managed thread), tools are executed via the PrimitiveExecutor chain, and knowledge entries are parsed and returned as context.

## Parameters

| Parameter      | Type   | Required | Default | Description                                                                              |
| -------------- | ------ | -------- | ------- | ---------------------------------------------------------------------------------------- |
| `item_type`    | string | yes      | —       | `"directive"`, `"tool"`, or `"knowledge"`                                                |
| `item_id`      | string | yes      | —       | Relative path from `.ai/<type>/` without extension (e.g., `"rye/core/create_directive"`) |
| `project_path` | string | yes      | —       | Absolute path to the project root                                                        |
| `parameters`   | dict   | no       | `{}`    | Parameters to pass to the item                                                           |
| `dry_run`      | bool   | no       | `false` | Validate without executing                                                               |
| `thread`       | string | no       | `"inline"` | Thread mode — `"inline"` (default, return content in-thread), `"fork"` (spawn a managed LLM thread, directives only), `"remote"` (remote server execution), or `"remote:name"` (named remote, e.g. `"remote:gpu"`) |
| `async`        | bool   | no       | `false` | Return immediately with thread_id instead of waiting. Works with directive+fork, directive+remote, tool+inline, and tool+remote combinations. |
| `model`        | string | no       | —       | For directives: override LLM model for thread execution         |
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

Three execution modes controlled by the `thread` parameter:

#### Inline mode (default, `thread="inline"`)

Validates inputs, interpolates placeholders, and returns the parsed directive content with an `instructions` field. The calling agent follows the steps in its own context. No LLM infrastructure required.

#### Fork mode (`thread="fork"`)

Validates inputs, then spawns a managed thread to execute the directive. The thread runs with its own LLM loop, safety harness, and budgets. If `async: true`, returns immediately with `thread_id` and `pid` instead of blocking.

#### Remote mode (`thread="remote"` or `thread="remote:name"`)

Validates inputs, then pushes execution to a remote ryeos server via the `rye/core/remote/remote` tool. The remote server materializes a `.ai/` directory from CAS manifests, runs the executor, and returns results. Use the `"remote:name"` syntax to target a specific named remote (e.g., `"remote:gpu"`). Named remotes are configured in `cas/remote.yaml`.

If `async: true`, the execution is wrapped in a detached child process via `_launch_async()` → `async_runner.py`, which re-enters `ExecuteTool.handle()` with the remote thread mode.

**Input validation (all modes):**

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

**In-thread response (default):**

```json
{
  "your_directions": "<interpolated directive body>"
}
```

**Fork response (`thread="fork"`):**

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

**Fork async response (`thread="fork", async=true`):**

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

**Async response (tool or remote directive):**

```json
{
  "status": "success",
  "async": true,
  "thread_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "type": "tool",
  "item_id": "rye/bash",
  "execution_mode": "inline",
  "state": "running",
  "pid": 42857
}
```

Async execution for tools and remote directives uses `_launch_async()`, which generates a UUID-based thread_id, registers in the ThreadRegistry (SQLite), spawns `async_runner.py` as a detached child process via `launch_detached()`, and returns immediately. Results are stored in the ThreadRegistry via `registry.set_result()`. Thread log dir is at `.ai/agent/threads/{thread_id}/`.

**Dry run:** Returns `"status": "validation_passed"` after parsing and input validation, without executing or spawning a thread.

**`<returns>` injection:** When a directive is executed in threaded mode, the infrastructure transforms the directive's `<outputs>` into a `<returns>` block appended to the end of the rendered prompt. This tells the LLM what structured output keys to produce. The LLM never sees the raw `<outputs>` XML — it sees the deterministically generated `<returns>` section after the process steps. See [Authoring Directives — How Outputs Become `<returns>`](../authoring/directives.md#how-outputs-become-returns-in-the-prompt) for details.

### Tools

Executes through the PrimitiveExecutor with recursive chain resolution:

1. **Build chain** — Resolves the tool's `__executor_id__` to find the runtime, then the runtime's executor to find the primitive. Produces a chain like: `tool → runtime → primitive`.
2. **Validate chain** — Checks space compatibility and I/O matching between chain elements.
3. **Resolve ENV_CONFIG** — Environment variables and secrets are resolved through the chain.
4. **Execute** — The root Lillux primitive (e.g., `subprocess`, `http_client`) runs the tool.

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

**Remote mode (`thread="remote"`):**

Tools can also be executed on a remote server. The execute tool pushes CAS objects, triggers remote execution via the `rye/core/remote/remote` tool, and pulls results back. Fork mode (`thread="fork"`) is not supported for tools — fork spawns managed LLM threads, which only apply to directives.

**Async mode (`async: true`):**

Any tool execution (inline or remote) can be made async. The tool is wrapped in a detached child process that runs `async_runner.py`. The parent returns immediately with a `thread_id` and `pid`. Results are stored in the ThreadRegistry.

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

All items are verified against their signature before execution. If an item has been modified since signing, or moved without re-signing, execution fails with an `IntegrityError`. Error messages include the item type, signing key details, and a concrete `rye sign` fix command.

Set `RYE_DEV_MODE=1` to downgrade integrity failures to warnings during development (see [Integrity — Dev Mode](../internals/integrity-and-signing.md#dev-mode)).

## Chain Trace

For tool execution, the `PrimitiveExecutor` supports a `trace` mode that returns detailed event logs alongside the result. Trace events show which files were resolved, which spaces were searched, what was shadowed, integrity verification results per chain element, and environment variable contributions.

See [Executor Chain — Chain Trace Mode](../internals/executor-chain.md#chain-trace-mode) for details.

## Async Validation Rules

Three combinations are rejected before dispatch:

| Combination | Why rejected |
|---|---|
| `async + dry_run` | Validation is instant — nothing to detach |
| `async + knowledge` | Knowledge loading is immediate — nothing to detach |
| `async + directive + inline` | Inline directives return text for the agent to follow — there is nothing to detach. Use `thread="fork"` for async directives. |

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

### Execute a directive (in-thread, default)

```python
# Returns parsed content for the calling agent to follow
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

### Execute a directive in a managed thread

```python
# Spawns a managed thread and blocks until completion
rye_execute(
    item_type="directive",
    item_id="my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin", "batch_size": 5},
    thread="fork"
)
```

### Execute a directive asynchronously in a managed thread

```python
# Returns immediately with thread_id
rye_execute(
    item_type="directive",
    item_id="my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin", "batch_size": 5},
    thread="fork",
    limit_overrides={"turns": 30, "spend": 3.00},
    async=True
)
```

### Execute a directive on a remote server

```python
rye_execute(
    item_type="directive",
    item_id="my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin"},
    thread="remote"
)
```

### Execute a directive on a named remote

```python
rye_execute(
    item_type="directive",
    item_id="my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin"},
    thread="remote:gpu"
)
```

### Execute a tool asynchronously

```python
# Returns immediately with thread_id — result stored in ThreadRegistry
rye_execute(
    item_type="tool",
    item_id="rye/bash",
    project_path="/home/user/my-project",
    parameters={"command": "python train.py --epochs 100"},
    async=True
)
```

### Execute a tool on a remote server

```python
rye_execute(
    item_type="tool",
    item_id="my-project/heavy-compute",
    project_path="/home/user/my-project",
    parameters={"data": "input.csv"},
    thread="remote:gpu"
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
    item_id="rye/bash",
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
