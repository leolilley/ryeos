```yaml
id: node-cache
title: "Node Execution Cache"
description: Skip re-execution when inputs haven't changed — opt-in per-node caching for state graph nodes
category: internals
tags: [cache, node-cache, state-graphs, performance]
version: "1.0.0"
```

# Node Execution Cache

## Overview

Node execution cache skips re-execution when inputs haven't changed. Opt-in per node via `cache_result: true` in graph YAML. Default is `false` (safe — external actions are not cached). Cache keys are deterministic hashes of the interpolated action plus config state.

## Cache Key Composition

The cache key is a SHA256 of canonical JSON containing:

```python
{
    "schema": 1,
    "kind": "node_input",
    "graph_hash": hash_of_graph_yaml,
    "node_name": "summarize",
    "interpolated_action": {
        # fully resolved by interpolation.interpolate_action()
        "item_type": "tool",
        "item_id": "rye/agent/threads/thread_directive",
        "params": {"directive": "summarize", "context": "actual resolved value"}
    },
    "config_snapshot_hash": "cd45...",
}
```

This means: same graph + same node + same resolved parameters + same config = cache hit.

The `config_snapshot_hash` comes from `compute_agent_config_snapshot()` in `rye/cas/config_snapshot.py`, which loads `agent.yaml`, `resilience.yaml`, `coordination.yaml`, and `hooks.yaml` via 3-tier merge (system → user → project), combines them, and hashes. Any config change invalidates all caches.

## Enabling Caching

Add `cache_result: true` to any node in a graph YAML:

```yaml
nodes:
  summarize:
    cache_result: true
    action:
      primary: execute
      item_type: tool
      item_id: rye/agent/threads/thread_directive
      params:
        directive: summarize
        context: "${state.document_text}"
    assign:
      summary: "${result.output}"
    next: store_results
```

## Cache Lookup and Store

`rye/cas/node_cache.py` provides three functions:

| Function | Description |
|----------|-------------|
| `compute_cache_key(graph_hash, node_name, interpolated_action, config_snapshot_hash) -> str` | Compute deterministic SHA256 cache key |
| `cache_lookup(cache_key, project_path) -> dict | None` | Returns `{"result": ..., "node_result_hash": ...}` on hit, `None` on miss |
| `cache_store(cache_key, result, project_path, node_name, elapsed_ms) -> str | None` | Store result as `NodeResult` CAS object, write cache pointer. Returns `node_result_hash`. |

Cache pointer files stored at `.ai/state/objects/cache/nodes/{cache_key}.json`:

```json
{
    "node_result_hash": "ef67...",
    "node_name": "summarize",
    "cached_at": 1709000000.0
}
```

The pointer references a `NodeResult` CAS object (immutable). On lookup, the pointer is read, then the `NodeResult` is fetched from CAS by hash.

## Walker Integration

In the graph walker's main execution loop, between interpolation and dispatch:

```
1. interpolate action from state (existing)
2. compute node_input hash                              ← cache
3. check cache: objects/cache/nodes/<hash>.json          ← cache
4. if hit: load node_result, skip _dispatch_action()     ← cache
5. if miss: dispatch, store node_result + receipt        ← cache
6. assign + evaluate edges (existing)
```

Cache errors are logged at warning level, never swallowed — a failed lookup falls through to normal execution.

## Config Snapshot (`rye/cas/config_snapshot.py`)

The config snapshot ensures cache invalidation when agent configuration changes.

| Function | Description |
|----------|-------------|
| `compute_config_hash(resolved_configs) -> str` | SHA256 of canonical JSON of merged config dict |
| `compute_agent_config_snapshot(project_path) -> (hash, configs)` | Load all 4 config files via 3-tier merge, return hash + resolved dict |

Config files included: `agent.yaml`, `resilience.yaml`, `coordination.yaml`, `hooks.yaml`. Each loaded via 3-tier resolution (system → user → project), deep-merged.

## What Is Cacheable

| Node type | Cacheable by default | Notes |
|-----------|---------------------|-------|
| `gate` | No | Pure routing, no execution cost |
| `return` | No | Terminal, no execution |
| `foreach` | Per-iteration | Each iteration cached separately |
| Action → pure tool | Yes (if `cache_result: true`) | Deterministic tool with no side effects |
| Action → `thread_directive` | Yes (if `cache_result: true`) | Include config_snapshot_hash in key |
| Action → external API | No | Side effects, time-sensitive |

## Cache Invalidation

Caches invalidate automatically when any input changes:

| Change | Effect |
|--------|--------|
| Graph YAML modified | `graph_hash` changes → all nodes miss |
| Node action params change | `interpolated_action` changes → that node misses |
| Config updated | `config_snapshot_hash` changes → affected nodes miss |
| Config file edited | `config_snapshot_hash` changes → all nodes miss |
| State from upstream changes | Different `interpolated_action` → miss |

No manual cache invalidation needed. Delete `.ai/state/objects/cache/nodes/` to force re-execution of everything.

## Implementation Files

| Component | File |
|-----------|------|
| Node cache | `ryeos/rye/cas/node_cache.py` |
| Config snapshot | `ryeos/rye/cas/config_snapshot.py` |
| Object model | `ryeos/rye/cas/objects.py` |
| Graph walker | `.ai/tools/rye/core/runtimes/state-graph/walker.py` |
