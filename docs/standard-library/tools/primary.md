```yaml
id: tools-primary
title: "Primary Actions"
description: The three core MCP tools — fetch, execute, sign — that everything in Rye OS builds on
category: standard-library/tools
tags: [tools, primary, fetch, execute, sign, mcp]
version: "2.0.0"
```

# Primary Actions

**Namespace:** `rye/`
**Runtime:** `python/script`

These are the three foundational MCP tools that Rye OS exposes. They map 1:1 to the MCP tool interface and are the same implementations exposed via MCP. Inside threads, these tools are dynamically registered in the LLM's tool palette via dynamic tool registration based on the directive's capability strings.

Inside threads, these tools are dynamically registered in the LLM's tool palette based on the directive's capability strings. The runner routes calls using a `_primary` field on each tool definition.

---

## `rye_fetch`

**Item ID:** `rye/fetch`

Resolve an item by ID or discover items by query. Operates in two modes depending on which parameters are provided.

### ID Mode

When `item_id` is provided, resolves and returns the item's full content. Can also copy items between spaces.

### Query Mode

When `query` and `scope` are provided, discovers items by keyword search. Supports full-text search with AND, OR, NOT operators, wildcards (`*`), and quoted phrases.

### Parameters

| Name          | Type    | Required | Default | Description                                                                                                               |
| ------------- | ------- | -------- | ------- | ------------------------------------------------------------------------------------------------------------------------- |
| `item_id`     | string  | ❌       | —       | Item ID (relative path without extension). Triggers **ID mode**.                                                          |
| `item_type`   | string  | ❌       | —       | `directive`, `tool`, or `knowledge`. Optional in ID mode (auto-detected); unused in query mode.                           |
| `source`      | string  | ❌       | —       | ID mode: space to load from (`project`, `user`, `system`). Query mode: space to search (`project`, `user`, `system`, `local`, `registry`, `all`). When omitted, cascades **project → user → system** (first match wins). |
| `destination` | string  | ❌       | —       | Copy to this space after resolving: `project` or `user`. ID mode only.                                                   |
| `query`       | string  | ❌       | —       | Keyword search query. Triggers **query mode**.                                                                            |
| `scope`       | string  | ❌       | —       | Item type and optional namespace filter. Query mode only.                                                                 |
| `limit`       | integer | ❌       | `10`    | Maximum results to return. Query mode only.                                                                               |

### Scope Format

Scopes use capability string format or shorthand:

| Scope                   | Meaning                       |
| ----------------------- | ----------------------------- |
| `directive`             | All directives                |
| `tool`                  | All tools                     |
| `knowledge`             | All knowledge                 |
| `tool.rye.core.*`       | Tools under `rye/core/`       |
| `directive.rye.agent.*` | Directives under `rye/agent/` |

### Examples

**ID mode — inspect an item:**

```python
rye_fetch(item_id="rye/core/create_directive")
```

**ID mode — restrict source and copy to project:**

```python
rye_fetch(item_id="rye/file-system/read", item_type="tool",
    source="system", destination="project")
```

**Query mode — search by keyword:**

```python
rye_fetch(scope="tool", query="file system operations")
rye_fetch(scope="knowledge", query="metadata specification", source="system")
```

---

## `rye_execute`

**Item ID:** `rye/execute`

Execute a directive, tool, or knowledge item. This is the universal execution entry point.

### Parameters

| Name         | Type    | Required | Default | Description                               |
| ------------ | ------- | -------- | ------- | ----------------------------------------- |
| `item_type`  | string  | ✅       | —       | `directive`, `tool`, or `knowledge`       |
| `item_id`    | string  | ✅       | —       | Item ID (relative path without extension) |
| `parameters` | object  | ❌       | `{}`    | Parameters to pass to the item            |
| `dry_run`    | boolean | ❌       | `false` | Validate without executing                |

### What Happens

1. Resolves the item across spaces (project → user → system)
2. Loads the item's metadata and determines the executor
3. For tools: runs the executor (Python runtime, Bash runtime, MCP runtime, etc.)
4. For directives: returns the parsed directive content for the LLM to follow
5. For knowledge: returns the entry content

### Example

```python
# Execute a tool
rye_execute(item_type="tool", item_id="rye/file-system/read",
    parameters={"file_path": "README.md"})

# Execute a directive (in a thread context)
rye_execute(item_type="directive", item_id="rye/core/create_tool",
    parameters={"tool_name": "my-tool", "category": "utils", "tool_type": "python"})

# Dry run — validate without executing
rye_execute(item_type="tool", item_id="rye/bash",
    parameters={"command": "echo test"}, dry_run=True)
```

---

## `rye_sign`

**Item ID:** `rye/sign`

Validate and sign an item file. Signing computes a content hash and cryptographic signature, embedding it in the file's header comment. Signed items can be verified for integrity.

### Parameters

| Name        | Type   | Required | Default   | Description                           |
| ----------- | ------ | -------- | --------- | ------------------------------------- |
| `item_type` | string | ✅       | —         | `directive`, `tool`, or `knowledge`   |
| `item_id`   | string | ✅       | —         | Item ID to sign                       |
| `source`    | string | ❌       | `project` | Space to sign in: `project` or `user` |

### Signature Format

The signature is embedded as the first line of the file:

**Python tools:**

```python
# rye:signed:<timestamp>:<content_hash>:<signature>:<key_id>
```

**Markdown directives/knowledge:**

```markdown
<!-- rye:signed:<timestamp>:<content_hash>:<signature>:<key_id> -->
```

### Example

```python
rye_sign(item_type="tool", item_id="my-category/my-tool")
rye_sign(item_type="directive", item_id="workflows/deploy")
```