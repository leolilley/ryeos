```yaml
id: execute
title: "rye_execute"
description: Execute directives or tools
category: tools-reference
tags: [execute, mcp-tool, api]
version: "2.0.0"
```

# rye_execute

Execute directives or tools. The `item_id` parameter accepts canonical refs (`directive:X` or `tool:X`) or plain IDs. Directives are validated and returned in-thread by default (set `thread="fork"` to spawn a managed thread). Tools are dispatched through the PrimitiveExecutor chain. Set `target="remote"` to execute on a remote server.

## Parameters

| Parameter         | Type   | Required | Default    | Description                                                                                                                                                                                                        |
| ----------------- | ------ | -------- | ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `item_id`         | string | yes      | —          | Canonical ref (`directive:X`, `tool:X`) or plain path. Resolved from `.ai/<type>/` without extension (e.g., `"tool:rye/bash"` or `"directive:rye/core/create_directive"`)                                          |
| `project_path`    | string | yes      | —          | Absolute path to the project root                                                                                                                                                                                  |
| `parameters`      | dict   | no       | `{}`       | Parameters to pass to the item                                                                                                                                                                                     |
| `dry_run`         | bool   | no       | `false`    | Validate without executing                                                                                                                                                                                         |
| `target`          | string | no       | `"local"`  | Where to execute — `"local"` (default, execute in current environment), `"remote"` (remote server execution), or `"remote:name"` (named remote, e.g. `"remote:gpu"`)                                              |
| `thread`          | string | no       | `"inline"` | How to execute — `"inline"` (default, return content in-thread) or `"fork"` (spawn a managed LLM thread, directives only)                                                                                         |
| `async`           | bool   | no       | `false`    | Return immediately with thread_id instead of waiting. Works with directive+fork, directive+remote, tool+inline, and tool+remote combinations.                                                                      |
| `model`           | string | no       | —          | For directives: override LLM model for thread execution                                                                                                                                                            |
| `limit_overrides` | object | no       | —          | For directives: override limits (`turns`, `tokens`, `spend`, `spawns`, `duration_seconds`, `depth`)                                                                                                                |

## Two-Path Execution Model

Execute routes items through two distinct paths based on type:

1. **Directives** — Validated, inputs interpolated, and either returned in-thread (`your_directions`) or dispatched to a managed LLM thread (`thread="fork"`). No executor chain involved.
2. **Tools** — Dispatched through the PrimitiveExecutor chain. The tool's `__executor_id__` is resolved to a runtime, then to a Lillux primitive, forming a signed chain that is validated and executed.

## Item Resolution

Items are resolved by searching three spaces in order: **project** → **user** → **system**.

```
project:  <project_path>/.ai/<type>/<item_id>.py
user:     <USER_SPACE>/.ai/<type>/<item_id>.py
system:   <rye-package>/.ai/<type>/<item_id>.py
```

File extensions are tried automatically based on item type. Directives use `.md`. Tools try `.py`, `.yaml`, `.yml`, `.js`, `.sh`, and others registered via extractors.

## Behavior by Item Type

### Directives

Three execution modes controlled by the `target` and `thread` parameters:

#### Inline mode (default, `thread="inline"`)

Validates inputs, interpolates placeholders, and returns the parsed directive content with an `instructions` field. The calling agent follows the steps in its own context. No LLM infrastructure required.

#### Fork mode (`thread="fork"`)

Validates inputs, then spawns a managed thread to execute the directive. The thread runs with its own LLM loop, safety harness, and budgets. If `async: true`, returns immediately with `thread_id` and `pid` instead of blocking.

#### Remote mode (`target="remote"` or `target="remote:name"`)

Validates inputs, then pushes execution to a remote ryeos server via the `rye/core/remote/remote` tool. The remote server materializes a `.ai/` directory from CAS manifests, runs the executor, and returns results. Directives require `thread="fork"` when targeting a remote server. Use the `"remote:name"` syntax on the `target` parameter to target a specific named remote (e.g., `"remote:gpu"`). Named remotes are configured in `remotes/remotes.yaml`.

If `async: true`, the execution is wrapped in a detached child process via `_launch_async()` → `async_runner.py`, which re-enters `ExecuteTool.handle()` with the remote target.

**Input validation (all modes):**

1. Declared inputs with `default` values are applied first
2. Required inputs without values produce an error with the full `declared_inputs` list
3. Placeholders are interpolated in `body`, `content`, `raw`, and all `actions`

**Input interpolation syntax:**

| Syntax                 | Behavior                                                  |
| ---------------------- | --------------------------------------------------------- |
| `{input:key}`          | Required — kept as-is if missing                          |
| `{input:key?}`         | Optional — replaced with empty string                     |
| `{input:key:default}`  | Fallback — uses `default` if key is missing               |
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

**Async response (tool):**

```json
{
  "status": "success",
  "async": true,
  "thread_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "type": "tool",
  "item_id": "tool:rye/bash",
  "execution_mode": "inline",
  "state": "running",
  "pid": 42857
}
```

Async execution for tools and remote directives uses `_launch_async()`, which generates a UUID-based thread_id, registers in the ThreadRegistry (SQLite), spawns `async_runner.py` as a detached child process via `launch_detached()`, and returns immediately. Results are stored in the ThreadRegistry via `registry.set_result()`. Thread log dir is at `.ai/state/threads/{thread_id}/`.

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
  "item_id": "tool:rye/file-system/write",
  "data": { "...execution output..." },
  "chain": ["rye/file-system/write", "rye/core/runtimes/python/script", "rye/core/primitives/execute"],
  "metadata": { "duration_ms": 45 }
}
```

**Remote mode (`target="remote"`):**

Tools can also be executed on a remote server. The execute tool pushes CAS objects, triggers remote execution via the `rye/core/remote/remote` tool, and pulls results back. The `target` parameter controls where execution happens. Fork mode (`thread="fork"`) is not supported for tools — fork spawns managed LLM threads, which only apply to directives.

**Async mode (`async: true`):**

Any tool execution (inline or remote) can be made async. The tool is wrapped in a detached child process that runs `async_runner.py`. The parent returns immediately with a `thread_id` and `pid`. Results are stored in the ThreadRegistry.

**Dry run:** Builds and validates the executor chain without executing. Returns chain details and validated pairs on success, or specific chain validation errors on failure.

```json
{
  "status": "validation_passed",
  "message": "Tool chain validation passed (dry run)",
  "item_id": "tool:rye/file-system/write",
  "chain": ["..."],
  "validated_pairs": ["..."]
}
```

## Integrity Verification

All items are verified against their signature before execution. If an item has been modified since signing, or moved without re-signing, execution fails with an `IntegrityError`. Error messages include the item type, signing key details, and a concrete `rye sign` fix command.

Set `RYE_DEV_MODE=1` to downgrade integrity failures to warnings during development (see [Integrity — Dev Mode](../internals/integrity-and-signing.md#dev-mode)).

## Chain Trace

For tool execution, the `PrimitiveExecutor` supports a `trace` mode that returns detailed event logs alongside the result. Trace events show which files were resolved, which spaces were searched, what was shadowed, integrity verification results per chain element, and environment variable contributions.

See [Executor Chain — Chain Trace Mode](../internals/executor-chain.md#chain-trace-mode) for details.

## Execution Matrix

Not all target/thread combinations apply to all item types. Invalid combinations are rejected with an error by `_validate_execution()` in `handle()`.

### Sync (`async: false`)

| Target / Thread | Directive                       | Tool                              |
| --------------- | ------------------------------- | --------------------------------- |
| local + inline  | ✅ Returns `your_directions`    | ✅ Executes via PrimitiveExecutor |
| local + fork    | ✅ Spawns managed LLM thread   | ❌ Fork is for directives only    |
| remote + fork   | ✅ Server spawns LLM thread    | ❌ Fork is for directives only    |
| remote + inline | ❌ Directives need fork         | ✅ Server runs inline             |

### Async (`async: true`)

| Target / Thread | Directive                | Tool                           |
| --------------- | ------------------------ | ------------------------------ |
| local + inline  | ❌ Nothing to detach     | ✅ Detached child process      |
| local + fork    | ✅ Detached fork         | ❌ Fork is for directives only |
| remote + fork   | ✅ Detached remote+fork  | ❌ Fork is for directives only |
| remote + inline | ❌ Directives need fork  | ✅ Detached remote+inline      |

### Where validation happens

| Rule                                  | Validated by                                                   |
| ------------------------------------- | -------------------------------------------------------------- |
| Invalid (target, thread, type)        | `_validate_execution()` in execute.py `handle()`               |
| `dry_run + remote`                    | `_validate_execution()` in execute.py `handle()`               |
| `async + dry_run`                     | `_validate_execution()` in execute.py `handle()`               |
| `async + directive + local + inline`  | `_validate_execution()` in execute.py `handle()`               |
| Remote tool defense-in-depth          | `_execute()` in remote.py validates thread matches type        |
| Server-side defense-in-depth          | `server.py` re-validates thread/type on `/execute`             |

## Error Responses

Errors return `"status": "error"` with an `error` message and `item_id`:

```json
{
  "status": "error",
  "error": "Missing required inputs: name, category",
  "item_id": "directive:rye/core/create_directive",
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
  "item_id": "tool:rye/file-system/write",
  "chain": ["rye/file-system/write"],
  "metadata": { "duration_ms": 8 }
}
```

## Examples

### Execute a directive (in-thread, default)

```python
# Returns parsed content for the calling agent to follow
rye_execute(
    item_id="directive:rye/core/create_directive",
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
    item_id="directive:my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin", "batch_size": 5},
    thread="fork"
)
```

### Execute a directive asynchronously in a managed thread

```python
# Returns immediately with thread_id
rye_execute(
    item_id="directive:my-project/run_pipeline",
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
    item_id="directive:my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin"},
    target="remote",
    thread="fork"
)
```

### Execute a directive on a named remote

```python
rye_execute(
    item_id="directive:my-project/run_pipeline",
    project_path="/home/user/my-project",
    parameters={"location": "Dunedin"},
    target="remote:gpu",
    thread="fork"
)
```

### Execute a tool asynchronously

```python
# Returns immediately with thread_id — result stored in ThreadRegistry
rye_execute(
    item_id="tool:rye/bash",
    project_path="/home/user/my-project",
    parameters={"command": "python train.py --epochs 100"},
    async=True
)
```

### Execute a tool on a remote server

```python
rye_execute(
    item_id="tool:my-project/heavy-compute",
    project_path="/home/user/my-project",
    parameters={"data": "input.csv"},
    target="remote:gpu"
)
```

### Execute a tool

```python
rye_execute(
    item_id="tool:rye/file-system/write",
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
    item_id="tool:rye/bash",
    project_path="/home/user/my-project",
    parameters={"command": "echo test"},
    dry_run=True
)
```
