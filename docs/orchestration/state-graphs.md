```yaml
id: state-graphs
title: "State Graphs"
description: Declarative, code-free multi-step workflows defined as YAML graph tools and walked by a runtime
category: orchestration
tags: [state-graphs, graphs, workflows, orchestration, declarative]
version: "1.0.0"
```

# State Graphs

State graphs are a declarative way to define multi-step workflows as YAML tool files. Instead of writing Python or orchestrating LLM threads, you define a graph of nodes and edges — the runtime walks the graph, executes actions at each node, and routes to the next node based on conditions. No code, no LLM reasoning — just data-driven execution.

## Core Concept

A state graph is a YAML tool file with `tool_type: graph` and `executor_id: rye/core/runtimes/state_graph_runtime`. The runtime reads the graph definition, starts at the entry node, executes each node's action, applies state mutations, and follows edges to the next node.

Key properties:

- **State is a signed knowledge item** persisted after each step. This means graphs are resumable and auditable — every intermediate state is saved.
- **Graph runs register in the thread registry** for status tracking, just like thread-based orchestration.
- **Nodes are action dicts** — `{primary, item_type, item_id, params}` — the same action format used everywhere else in Rye.
- **Edges use `condition_evaluator`** for conditional routing. Edges are evaluated in order; first match wins.

## Graph Definition Format

A graph tool YAML has the same structure as any Rye tool — `config_schema` for input validation, plus `config` for the graph definition:

```yaml
version: "1.0.0"
tool_type: graph
executor_id: rye/core/runtimes/state_graph_runtime
category: workflows/example
description: "Example graph tool"

config_schema:
  type: object
  properties:
    directory:
      type: string
      default: "."

config:
  start: first_node # Entry node
  max_steps: 10 # Safety limit — prevents infinite loops

  nodes:
    first_node:
      action: # Action dict — same format as everywhere in Rye
        primary: execute
        item_type: tool
        item_id: rye/bash/bash
        params:
          command: "echo hello"
      assign: # State mutations — write action results into state
        greeting: "${result.stdout}"
      next: second_node # Unconditional edge — string

    second_node:
      action:
        primary: execute
        item_type: tool
        item_id: rye/bash/bash
        params:
          command: "echo ${state.greeting}"
      next: # Conditional edges — list of {to, when}
        - to: happy_path
          when:
            path: "state.greeting"
            op: "contains"
            value: "hello"
        - to: fallback # Last entry without `when` acts as default

    happy_path:
      type: return # Terminates the graph and returns state

    fallback:
      type: return
```

### Node Types

| Type        | Behavior                                                    |
| ----------- | ----------------------------------------------------------- |
| _(default)_ | Execute `action`, apply `assign`, follow `next`             |
| `return`    | Terminate the graph and return current state                |
| `foreach`   | Iterate over a collection, executing the action per item    |

### Foreach Nodes

Foreach nodes iterate over a list in state, executing an action for each item:

```yaml
fan_out:
  type: foreach
  over: "${state.tasks}"         # Expression resolving to a list
  as: task                       # Variable name for current item
  action:                        # Action to execute per item
    primary: execute
    item_type: tool
    item_id: rye/agent/threads/thread_directive
    params:
      directive_name: my/directive
      inputs:
        text: "${task.text}"
        output_path: "${task.output_path}"
  collect: results               # Optional — collect return values into state
  next: process_results
```

- **`over`** — Expression resolving to a list (e.g., `${state.items}`)
- **`as`** — Variable name bound to each item during iteration
- **`action`** — Standard action dict executed per item
- **`collect`** — Optional state key to store collected results as a list
- **Parallel mode** — When the action has `async_exec: true` in params, iterations dispatch concurrently via `asyncio.gather`

Items in `over` can be dicts, enabling dotted access: if `task` is `{text: "...", path: "..."}`, then `${task.text}` resolves correctly.

### Edge Formats

| Format               | Example           | Behavior                                 |
| -------------------- | ----------------- | ---------------------------------------- |
| String               | `next: done`      | Unconditional — always go to `done`      |
| List of `{to, when}` | See above         | Conditional — first matching `when` wins |
| Omitted              | _(no `next` key)_ | Implicit return — graph terminates       |

## How It Works (The Execution Chain)

State graphs follow the same execution chain pattern as any Rye tool:

```
graph tool YAML  →  state_graph_runtime  →  subprocess primitive
(nodes/edges)       (walks graph,              (runs the walker
                     dispatches rye_execute)     Python script)
```

This mirrors the standard tool chain — e.g., `my_tool.py → python_function_runtime → subprocess`. The runtime YAML uses an inline `-c` script that locates `state_graph_walker.py` via `{runtime_lib}` (the anchor lib path).

The walker:

1. Loads the graph definition from the tool YAML
2. Initializes or resumes state
3. Loops: execute current node's action → apply assign → evaluate edges → move to next node
4. Persists state after each step
5. Terminates on `type: return`, missing `next`, or `max_steps` exceeded

## Interpolation

Graph templates use `${dotted.path}` syntax — System 3 from the templating docs.

### Available Namespaces

| Namespace     | Contents                                                               |
| ------------- | ---------------------------------------------------------------------- |
| `${state.*}`  | Current graph state (accumulated `assign` values)                      |
| `${inputs.*}` | Graph input parameters (from `config_schema`)                          |
| `${result.*}` | Output of the current node's action (available in `assign` and `next`) |

### Path Resolution

Dotted paths support both dict key lookups and numeric list indices:

```yaml
# Dict access
command: "echo ${state.greeting}"

# List index access
code: "${state.file_contents.0.stdout}"    # First item's stdout
path: "${state.files.1.analysis_path}"     # Second item's analysis_path

# Nested — list item is a dict
text: "${state.tasks.2.text}"              # Third task's text field
```

### Type Preservation

When `${path}` is the **entire expression** (the whole string value), the resolved type is preserved:

```yaml
assign:
  count: "${result.stdout}" # If stdout is an int, count is an int
  items: "${result.data}" # If data is a list, items is a list
```

When `${path}` appears **inside** a larger string, string conversion is used:

```yaml
params:
  command: "Found ${state.count} files" # Always a string
```

## Permissions

The graph walker enforces capabilities via `_check_permission()` — the same capability system used throughout Rye.

- **Empty capabilities = deny all.** The walker is fail-closed. If no capabilities are passed, every action is rejected.
- **Pass capabilities as parameters** when invoking the graph:

```python
rye_execute(
    item_type="tool",
    item_id="my-project/workflows/my_graph",
    parameters={
        "directory": ".",
        "capabilities": ["rye.execute.tool.*"],
        "depth": 5
    }
)
```

- **Capability format:** `rye.<primary>.<item_type>.<dotted.item.id>` with fnmatch wildcards

| Example                           | Grants                                 |
| --------------------------------- | -------------------------------------- |
| `rye.execute.tool.*`              | Execute any tool                       |
| `rye.execute.tool.rye.bash.bash`  | Execute only `rye/bash/bash`           |
| `rye.load.knowledge.my-project.*` | Load any knowledge under `my-project/` |

## Result Unwrapping

When a node's action executes a tool, the raw result is an `ExecuteTool` envelope:

```json
{"status": "success", "type": "tool", "item_id": "rye/bash/bash", "data": {"stdout": "42", "stderr": "", "exit_code": 0}, "chain": [...], "metadata": {...}}
```

The walker **unwraps** this envelope — it lifts `data` to the top level and drops envelope keys (`chain`, `metadata`, `path`, `source`, `resolved_env_keys`). After unwrapping:

```json
{ "stdout": "42", "stderr": "", "exit_code": 0 }
```

This is why `${result.stdout}` in `assign` expressions works naturally — you reference the inner data fields directly.

### Error Propagation

If the outer envelope has `status: "error"` **or** the inner data has `success: false`, the unwrapped result gets `status: "error"` injected. This ensures the walker's error handling (`on_error` edges, hooks, `error_mode`) fires correctly for tool failures like non-zero bash exit codes.

## State Persistence

State is saved as a knowledge item after each step:

- **Path:** `.ai/knowledge/graphs/<graph_id>/<run_id>.md`
- **Format:** YAML frontmatter (status, current_node, step_count, timestamps) + JSON body (full state)
- **Signed** after each write via the standard Rye signing mechanism

### Resume

To resume a failed or interrupted graph run, pass `resume_run_id`:

```python
rye_execute(
    item_type="tool",
    item_id="my-project/workflows/my_graph",
    parameters={
        "directory": ".",
        "resume_run_id": "my_graph-1739820456"
    }
)
```

The walker loads the persisted state and continues from the last saved node.

## Error Handling

### Graph-Level Error Policy

Set `on_error` in `config` to control the default behavior for all nodes:

| Policy                       | Behavior                                              |
| ---------------------------- | ----------------------------------------------------- |
| `on_error: "fail"` (default) | Stop the graph on the first error                     |
| `on_error: "continue"`       | Skip `assign`, proceed to `next` edge evaluation      |

### Node-Level Error Edges

Individual nodes can define `on_error: <node_name>` to route to a recovery node when the action fails. This takes priority over the graph-level policy:

```yaml
nodes:
  risky_step:
    action:
      primary: execute
      item_type: tool
      item_id: rye/bash/bash
      params:
        command: "might-fail"
    on_error: handle_error     # Route to recovery node on failure
    next: success_path         # Normal path on success

  handle_error:
    action:
      primary: execute
      item_type: tool
      item_id: rye/bash/bash
      params:
        command: "echo 'recovered'"
    assign:
      recovery_note: "Error was caught"
    next: success_path
```

When an error occurs, `state._last_error` is populated with `{node, error}` regardless of which error handling path is taken.

### Hook-Based Retry

`error` hooks can return a retry action:

```yaml
config:
  hooks:
    - event: error
      action:
        retry: true
        max_retries: 3
```

## Hooks

State graphs use the same hook infrastructure as directives.

### Supported Events

| Event             | Fires When                                           |
| ----------------- | ---------------------------------------------------- |
| `graph_started`   | Graph execution begins                               |
| `after_step`      | A node completes (action + assign + edge evaluation) |
| `error`           | A node action fails                                  |
| `limit`           | `max_steps` reached                                  |
| `graph_completed` | Graph terminates (via `return` node or final edge)   |

### Declaration

Hooks are declared in `config.hooks` as a list of `{event, condition, action}` objects:

```yaml
config:
  hooks:
    - event: after_step
      condition:
        path: "state.step_count"
        operator: "gte"
        value: 5
      action:
        primary: execute
        item_type: tool
        item_id: rye/bash/bash
        params:
          command: "echo 'Reached step 5'"
```

## Example: Project Stats

A complete graph that counts Python files, counts total lines, and returns the results:

```yaml
version: "1.0.0"
tool_type: graph
executor_id: rye/core/runtimes/state_graph_runtime
category: workflows/test
description: "Gather project stats: count Python files, count total lines, produce summary"

config_schema:
  type: object
  properties:
    directory:
      type: string
      default: "."

config:
  start: count_files
  max_steps: 10

  nodes:
    count_files:
      action:
        primary: execute
        item_type: tool
        item_id: rye/bash/bash
        params:
          command: "find ${inputs.directory} -name '*.py' -not -path '*/.venv/*' | wc -l"
      assign:
        file_count: "${result.stdout}"
      next: count_lines

    count_lines:
      action:
        primary: execute
        item_type: tool
        item_id: rye/bash/bash
        params:
          command: "find ${inputs.directory} -name '*.py' -not -path '*/.venv/*' -exec cat {} + 2>/dev/null | wc -l"
      assign:
        line_count: "${result.stdout}"
      next: done

    done:
      type: return
```

Run it:

```python
rye_execute(
    item_type="tool",
    item_id="my-project/workflows/project_stats",
    parameters={
        "directory": ".",
        "capabilities": ["rye.execute.tool.rye.bash.bash"],
        "depth": 5
    }
)
```

Result: `{"file_count": "42", "line_count": "1337", "_status": "completed"}`.

## State Graphs vs Thread Orchestration

|                 | State Graphs                                             | Thread Orchestration                                   |
| --------------- | -------------------------------------------------------- | ------------------------------------------------------ |
| **Flow**        | Deterministic — edges defined in YAML                    | LLM-driven — model reasons about next step             |
| **Best for**    | Data-driven pipelines where the flow is known in advance | Workflows that need reasoning, judgment, or adaptation |
| **Cost**        | Minimal — no LLM calls for routing                       | Higher — LLM calls at every step                       |
| **Flexibility** | Fixed graph structure                                    | Fully dynamic — model can change course                |
| **Debugging**   | Read the YAML, follow the edges                          | Read the transcript                                    |

**Combining them:** Graph nodes can spawn `thread_directive` children for steps that need LLM reasoning. Use graphs for the deterministic scaffold and threads for the intelligent steps.

## Async Graph Execution

Graphs support `async_exec: true` — same pattern as `thread_directive`. The caller returns immediately with a `graph_run_id` while the graph runs in the background.

```python
rye_execute(
    item_type="tool",
    item_id="my-project/workflows/my_graph",
    parameters={
        "directory": ".",
        "async_exec": True,
        "capabilities": ["rye.execute.tool.*"],
        "depth": 5
    }
)
# Returns immediately:
# {"success": true, "graph_run_id": "my_graph-1739820456", "status": "running", "pid": 12345}
```

The background process:
- Forks via `os.fork()` and daemonizes (`os.setsid()`)
- Runs the graph to completion, updating the thread registry
- Writes stderr to `.ai/threads/<graph_run_id>/async.log` for debugging
- Updates registry status to `completed` or `error` on finish

Monitor progress by querying the thread registry or checking the persisted state knowledge item.

## What's Next

- [Thread Lifecycle](./thread-lifecycle.md) — How threads are created, executed, and finalized
- [Permissions and Capabilities](./permissions-and-capabilities.md) — Capability tokens and fail-closed security
- [Building a Pipeline](./building-a-pipeline.md) — Step-by-step tutorial for thread-based orchestration

## Implementation Files

| Component           | File                                                         |
| ------------------- | ------------------------------------------------------------ |
| Graph walker        | `.ai/tools/rye/core/runtimes/state_graph_walker.py`          |
| Runtime YAML        | `.ai/tools/rye/core/runtimes/state_graph_runtime.yaml`       |
| Interpolation       | `.ai/tools/rye/agent/threads/loaders/interpolation.py`       |
| Condition evaluator | `.ai/tools/rye/agent/threads/loaders/condition_evaluator.py` |
| Thread registry     | `.ai/tools/rye/agent/threads/persistence/thread_registry.py` |
