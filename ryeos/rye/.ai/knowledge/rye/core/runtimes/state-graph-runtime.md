<!-- rye:signed:2026-02-22T23:38:13Z:27d62b1d30c4f097410152a5b492c4fee8b42954601eb22a6b1b3182530a0ac7:M2fswUaxXVsxJYeoH1YVfXzAAS6J6G6lBCDELYuP3qflWgNyw9P8_DkKdCHykNc63TShIoSeWsM01leFdneuAw==:9fbfabe975fa5a7f -->
```yaml
id: state-graph-runtime
title: "State Graph Runtime"
description: Runtime that walks declarative graph YAML tools, dispatching rye_execute for each node
entry_type: reference
category: rye/core/runtimes
tags: [runtime, graph, state-graph, walker, orchestration]
version: "1.0.0"
```

# State Graph Runtime

The state graph runtime (`rye/core/runtimes/state_graph_runtime`) enables declarative, code-free workflow definitions as YAML graph tools. It sits at Layer 2 of the executor chain, between graph tool YAMLs (Layer 3) and the subprocess primitive (Layer 1).

## Chain Pattern

```
graph tool YAML  →  state_graph_runtime.yaml  →  subprocess primitive
(nodes/edges)       (inline -c script loads       (runs Python process)
                     state_graph_walker.py)
```

## How the Walker Is Located

The runtime YAML uses an inline Python `-c` script that receives `{runtime_lib}` as `sys.argv[4]`. Since `runtime_lib` resolves to `<runtime_dir>/lib/python` (from the anchor config's `lib` field), the script navigates up two directories to find `state_graph_walker.py` in the same directory as the runtime YAML.

## Anchor Configuration

The runtime uses `mode: always` (not `auto`) because graph tool YAMLs typically lack marker files (`__init__.py`, `pyproject.toml`) in their directories. The anchor always activates to ensure `{runtime_lib}` is computed and `PYTHONPATH` includes the walker's dependencies in `lib/python/`.

## Key Design Decisions

1. **Anchor context injection** — `PrimitiveExecutor` injects anchor context variables (`runtime_lib`, `anchor_path`, `tool_dir`, `tool_parent`) into execution parameters so they're available as `{param}` template variables in subprocess args.

2. **Config key collision prevention** — Tool parameters named `command` or `args` are excluded from the config merge to prevent overriding the runtime's subprocess command. They remain accessible through `{params_json}`.

3. **Result unwrapping** — The walker lifts the `data` dict from `ExecuteTool` responses to the top level so graph assign expressions like `${result.stdout}` work directly. Error propagation: if the outer envelope has `status: "error"` or the inner data has `success: false`, `status: "error"` is injected into the unwrapped result so `on_error` edges and hooks fire correctly.

4. **Async execution** — `run_sync()` wrapper handles `async_exec: true` via `os.fork()`, same pattern as `thread_directive.py`. Parent returns immediately with `{graph_run_id, status: "running"}`, child daemonizes and runs to completion. The graph_run_id is pre-generated and pre-registered before the fork to avoid duplicate registry entries.

5. **List index resolution** — `condition_evaluator.resolve_path()` supports numeric list indices in dotted paths (`state.items.0.name`), enabling foreach patterns where collected results are accessed by position.

## Implementation Files

| File                                                      | Purpose                                          |
| --------------------------------------------------------- | ------------------------------------------------ |
| `.ai/tools/rye/core/runtimes/state_graph_runtime.yaml`    | Runtime config (anchor, env, inline loader)      |
| `.ai/tools/rye/core/runtimes/state_graph_walker.py`       | Graph traversal engine (~1240 lines)             |
| `.ai/tools/rye/core/runtimes/lib/python/module_loader.py` | Module loading utilities                         |
| `.ai/tools/rye/agent/threads/loaders/condition_evaluator.py` | Path resolution with list index support       |
| `rye/executor/primitive_executor.py`                      | Anchor context injection, error msg fallback     |
