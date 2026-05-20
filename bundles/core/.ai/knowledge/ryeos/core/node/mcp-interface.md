---
category: ryeos/core
tags: [fundamentals, mcp, agent-interface, tools]
version: "1.0.0"
description: >
  How AI agents interact with Rye OS via MCP — the three tools
  (execute, fetch, sign), command dispatch, and routing patterns.
---

# MCP Agent Interface

AI agents (opencode, amp, Claude, etc.) interact with Rye OS through
**MCP (Model Context Protocol)** tools. The MCP server (`ryeosd-mcp`)
exposes three tools that map directly to the core CLI verbs.

## The Three MCP Tools

### `execute` — Run Items
Execute a directive, tool, service, graph, or knowledge operation.

**Parameters:**
| Field          | Type   | Required | Description                    |
|----------------|--------|----------|--------------------------------|
| `item_id`      | string | yes      | Canonical ref or bare ID       |
| `parameters`   | object | no       | Input values for the item      |
| `project_path` | string | yes      | Absolute path to project root  |
| `thread`       | string | no       | `"inline"` (default) or `"fork"` |
| `async`        | bool   | no       | Fire-and-forget (fork mode only) |
| `dry_run`      | bool   | no       | Validate without executing     |
| `target`       | string | no       | `"local"` or `"remote"`        |
| `resume_thread_id` | string | no   | Resume a paused thread         |

**Returns:** Execution result, or thread_id if async.

For directives, the response contains `your_directions` (instructions
for the calling agent to follow) and `body` (the directive's prompt).

### `fetch` — Read Items
Resolve and read an item without executing it.

**Parameters:**
| Field          | Type   | Required | Description                    |
|----------------|--------|----------|--------------------------------|
| `item_id`      | string | yes*     | Canonical ref or bare ID       |
| `query`        | string | no*      | Search query (discovery mode)  |
| `scope`        | string | no       | Filter by type/namespace       |
| `project_path` | string | yes      | Absolute path to project root  |
| `source`       | string | no       | `project`, `user`, `system`, `all` |
| `destination`  | string | no       | Copy to `project` or `user`    |
| `with_content` | bool   | no       | Include file body              |
| `verify`       | bool   | no       | Also check signature           |

*One of `item_id` or `query` is required.

**Two modes:**
1. **ID mode** (`item_id` given) — resolve and return the item
2. **Query mode** (`query` given) — search and return matching items

### `sign` — Sign Items
Cryptographically sign an item after creation or edit.

**Parameters:**
| Field          | Type   | Required | Description                    |
|----------------|--------|----------|--------------------------------|
| `item_id`      | string | yes      | Canonical ref or glob pattern  |
| `project_path` | string | yes      | Absolute path to project root  |
| `source`       | string | no       | `project` (default) or `user`  |

Supports glob patterns for batch signing: `directive:*`, `tool:my/*`.

## Command Dispatch Table

Agents should map natural language to MCP tool calls:

| User Says                 | Tool     | Parameters                           |
|---------------------------|----------|--------------------------------------|
| "execute directive X"     | execute  | `item_id="directive:X"`              |
| "execute tool X"          | execute  | `item_id="tool:X"`                   |
| "fetch directive X"       | fetch    | `item_id="directive:X"`              |
| "search directives for X" | fetch    | `scope="directive", query="X"`       |
| "sign directive X"        | sign     | `item_id="directive:X"`              |
| "sign all tools"          | sign     | `item_id="tool:*"`                   |

## Fork Mode

For long-running directives, use `thread="fork"` to spawn a managed
background thread with its own LLM loop:

```
execute(item_id="directive:my/pipeline", thread="fork", async=true)
→ returns thread_id immediately
→ directive runs in background
```

Use `execute` with `resume_thread_id` to resume a paused thread.

## Modifier Reference

| Modifier         | Meaning                                              |
|------------------|------------------------------------------------------|
| `from system`    | `source="system"`                                    |
| `from user`      | `source="user"`                                      |
| `from project`   | `source="project"`                                   |
| `to user`        | `destination="user"` (copies item)                   |
| `to project`     | `destination="project"` (copies item)                |
| `dry run`        | `dry_run=true`                                       |
| `with {...}`     | `parameters={...}`                                   |

## Workflow Patterns

### Pattern 1: Execute and Follow
```
agent: execute(item_id="directive:deploy")
→ response: your_directions + body
→ agent follows the instructions in your_directions
```

### Pattern 2: Discover First
```
agent: fetch(query="testing", scope="directive")
→ response: list of matching directives
→ user picks one
→ agent: execute(item_id="directive:chosen-one")
```

### Pattern 3: Read-Only Inspection
```
agent: fetch(item_id="tool:my/helper", with_content=true)
→ response: tool metadata + source code
→ agent shows to user, does not execute
```
