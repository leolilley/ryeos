# Dynamic Tool Registration

## Problem

The current thread system passes 4 primary tools (`rye_execute`, `rye_search`, `rye_load`, `rye_sign`) as API-level tools, then injects a `<capabilities>` text block into the system prompt listing resolved tools like `ls(path)`, `read(path*)` as informational signatures.

Dumb models (Haiku) see those signatures and try to call them directly — `rye_file-system.ls(path: ".")` — instead of routing through `rye_execute(item_type="tool", item_id="rye/file-system/ls", parameters={path: "."})`. The model wastes turns on permission errors because those aren't real API tools.

Turn 1 also fails because `rye_execute`'s description says "Run a Rye item (directive, tool, or knowledge)" but only `tool` is granted, so the model tries `item_type="directive"`.

## Proposed Change

Register each resolved tool as a **real API-level tool** with a flattened name. The runner's dispatcher already handles the mapping — it just needs entries in `tool_id_map`.

### Before (current)

```
API tools: [rye_execute, rye_search, rye_load, rye_sign]
System prompt: <capabilities>
  rye_execute(item_type*, item_id*, parameters, dry_run) — Run a Rye item
    tools:
    └─ rye/file-system:
       ├─ ls(path) — List directory contents
       └─ read(path*, offset, limit) — Read file content
  rye_search(query*, scope*) — Discover item IDs
</capabilities>

LLM calls: rye_file-system.ls({path: "."})  ← FAILS (not a real tool)
```

### After (proposed)

```
API tools: [rye_search, rye_load, rye_sign,
            rye_file_system_ls, rye_file_system_read, ...]
            ↑ all peers — no rye_execute wrapper
Transcript metadata: capabilities list (for debugging/visibility)

LLM calls: rye_file_system_ls({path: "."})  ← WORKS (real tool, dispatcher routes to execute primary)
LLM calls: rye_search({query: "*", scope: "directive"})  ← WORKS (dispatcher routes to search primary)
```

## Key Distinction: External MCP vs Internal Thread

```
External MCP interface (unchanged):
  mcp__rye__execute   — public API for outside callers (Amp, Claude, etc.)
  mcp__rye__search
  mcp__rye__load
  mcp__rye__sign

Internal thread agent (this change):
  rye_search          — registered as API tool, dispatcher routes to search primary
  rye_load            — registered as API tool, dispatcher routes to load primary
  rye_sign            — registered as API tool, dispatcher routes to sign primary
  rye_file_system_ls  — registered as API tool, dispatcher routes to execute primary
  rye_file_system_read
  rye_bash
  ...
```

The external MCP surface stays the same 4 tools. Internally, the thread agent
sees a flat list of dynamically constructed tools — ALL of them peers, ALL
routed through the dispatcher to the correct primary action underneath. No
tool wraps another tool from the LLM's perspective.

## What Changes

### 1. `tool_schema_loader.py` — return tool defs instead of prompt text

Instead of building a `<capabilities>` XML string, return structured tool
definitions. Each tool carries a `_primary` field so the dispatcher knows
which primary action to route to.

The `_primary` value comes from the capability string that granted the tool.
`_classify_capability` already extracts this — the action field IS the primary:
- `rye.search.*` → action `search` → `_primary: "search"`
- `rye.load.*` → action `load` → `_primary: "load"`
- `rye.sign.*` → action `sign` → `_primary: "sign"`
- `rye.execute.tool.rye.file-system.*` → action `execute` → `_primary: "execute"`

No special treatment for any tool. `rye/search.py` is resolved the same way
as `rye/file-system/ls.py` — schema extracted, name flattened, `_primary`
field assigned. Both the MCP server and the primary tool files already import
descriptions from `ryeos/rye/primary_tool_descriptions.py` (single source
of truth), so no new wiring needed there.

```python
# Current return:
{"schemas": "<capabilities>...</capabilities>", "preloaded_tools": [...]}

# New return:
{"tool_defs": [
    # ALL tools are peers — no distinction
    {
        "name": "rye_search",                 # from rye/search.py
        "description": "...",                 # from CONFIG_SCHEMA (imports primary_tool_descriptions)
        "schema": {...},
        "_item_id": "rye/search",
        "_primary": "search",
    },
    {
        "name": "rye_file_system_ls",         # from rye/file-system/ls.py
        "description": "List directory contents",
        "schema": {...},
        "_item_id": "rye/file-system/ls",
        "_primary": "execute",
    },
    ...
],
"capabilities_summary": [...]}  # for transcript metadata, not injected into prompt
```

### 2. `thread_directive.py` — build available_tools from preload result

```python
# Line ~782-798 currently:
harness.available_tools = _build_tool_schemas()          # all 4 primary tools (unfiltered!)
preload_result = tool_schema_loader.preload_tool_schemas(...)
system_prompt += preload_result["schemas"]               # prompt injection

# Becomes:
preload_result = tool_schema_loader.preload_tool_schemas(...)
harness.available_tools = preload_result["tool_defs"]    # everything the agent can call
# Write capabilities_summary to transcript metadata for visibility
transcript.set_metadata("capabilities", preload_result["capabilities_summary"])
```

No more `_build_tool_schemas()`. The loader resolves ALL tools uniformly —
primary tools are just tools whose `_primary` field happens to be their own
action instead of "execute".

### 3. `runner.py` — unified dispatch via `_primary` field

The `tool_id_map` at line 82 already maps `name → _item_id`. Add a
parallel `tool_primary_map` for routing:

```python
tool_id_map = {
    t["name"]: t["_item_id"]
    for t in harness.available_tools
    if "_item_id" in t
}
tool_primary_map = {
    t["name"]: t["_primary"]
    for t in harness.available_tools
    if "_primary" in t
}
```

The dispatch block becomes uniform for ALL tools:

```python
resolved_id = tool_id_map.get(tc_name, tc_name)
primary = tool_primary_map.get(tc_name, "execute")

if primary == "execute":
    # Tool execution — params go straight through
    result = await dispatcher.dispatch({
        "primary": "execute",
        "item_type": "tool",
        "item_id": resolved_id,
        "params": dict(tool_call["input"]),
    })
else:
    # search/load/sign — params are the tool input directly
    result = await dispatcher.dispatch({
        "primary": primary,
        **tool_call["input"],
    })
```

#### Permission check simplifies

No more parsing `tc_input.get("item_type")`. Every tool carries its routing
info in the maps:

```python
tc_name = tool_call["name"]
primary = tool_primary_map.get(tc_name, "execute")
item_id = tool_id_map.get(tc_name, tc_name)

if primary == "execute":
    denied = harness.check_permission("execute", "tool", item_id)
elif primary == "search":
    # search permission uses scope from input
    scope = tc_input.get("scope", "")
    item_type_from_scope = scope.split(".")[0] if scope else ""
    denied = harness.check_permission("search", item_type_from_scope, "*")
else:
    denied = harness.check_permission(primary, tc_input.get("item_type", ""), item_id)
```

## Naming Convention

Straight flatten — no stripping, no special cases.

Primary tools move out of `rye/primary/rye_*.py` → `rye/*.py`:
- `rye/primary/rye_search.py` → `rye/search.py`
- `rye/primary/rye_load.py` → `rye/load.py`
- `rye/primary/rye_sign.py` → `rye/sign.py`
- `rye/primary/rye_execute.py` → `rye/execute.py` (kept for MCP dispatch, not registered internally)

Results:
- `rye/search` → `rye_search`
- `rye/load` → `rye_load`
- `rye/sign` → `rye_sign`
- `rye/file-system/ls` → `rye_file_system_ls`
- `rye/file-system/read` → `rye_file_system_read`
- `rye/bash` → `rye_bash` (move `rye/bash/bash.py` → `rye/bash.py`)

```python
def _tool_id_to_api_name(tool_id: str) -> str:
    return tool_id.replace("/", "_").replace("-", "_")
```

## Capabilities in Transcript Metadata

The capabilities list is still generated but written to transcript metadata
instead of injected into the system prompt. This gives us visibility into
what the agent was granted without polluting the prompt:

```yaml
# In transcript YAML header:
capabilities:
  - rye_search (directives, tools, knowledge)
  - rye_load (directives, tools, knowledge)
  - rye_sign (directives, tools, knowledge)
  - rye_file_system_ls
  - rye_file_system_read
  - rye_file_system_write
  - rye_file_system_glob
  - rye_file_system_grep
  - rye_file_system_edit_lines
  - rye_bash
```

## Token Budget Concern

More API tools = more token usage in the tools array. The current `max_tokens`
budget for the capabilities block should translate to a limit on how many
dynamic tools get registered. The schema extraction already exists — we're
just changing where the output goes (API tools array vs prompt text).

## Files to Change

1. `ryeos/bundles/standard/ryeos_std/.ai/tools/rye/primary/` → `rye/`
   - Move `rye_search.py` → `rye/search.py`, `rye_load.py` → `rye/load.py`, etc.
   - Move `rye/bash/bash.py` → `rye/bash.py`
   - Already import from `primary_tool_descriptions.py` — no schema changes needed
2. `ryeos/bundles/standard/ryeos_std/.ai/tools/rye/agent/threads/loaders/tool_schema_loader.py`
   - Return `tool_defs` list instead of `schemas` string
   - Resolve ALL tools uniformly (no primary/non-primary distinction)
   - Flatten names via `_tool_id_to_api_name()`, add `_primary` field
   - Return `capabilities_summary` for transcript metadata
3. `ryeos/bundles/standard/ryeos_std/.ai/tools/rye/agent/threads/thread_directive.py` (~line 782-798)
   - Remove `_build_tool_schemas()`
   - Set `harness.available_tools` from preload result
   - Write capabilities to transcript metadata
4. `ryeos/bundles/standard/ryeos_std/.ai/tools/rye/agent/threads/runner.py` (~line 467-568)
   - Add `tool_primary_map` alongside `tool_id_map`
   - Unified dispatch: route based on `_primary` field
   - Simplify permission check
5. `tests/engine/unit/test_tool_schema_preload.py` (update assertions)
