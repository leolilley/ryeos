```yaml
id: tools-primary
title: "Primary Tools"
description: The four core MCP tools — search, load, execute, sign — that everything in Rye OS builds on
category: standard-library/tools
tags: [tools, primary, search, load, execute, sign, mcp]
version: "1.0.0"
```

# Primary Tools

**Namespace:** `rye/primary/`
**Runtime:** `python/script`

These are the four foundational MCP tools that Rye OS exposes. They map 1:1 to the MCP tool interface — when you call `rye_search`, `rye_load`, `rye_execute`, or `rye_sign` in an MCP client, these are the tools being invoked.

Inside threads, these same tools are the ones the LLM calls. The thread's `tool_dispatcher` routes LLM tool calls to these implementations.

---

## `rye_search`

**Item ID:** `rye/primary/rye_search`

Search for directives, tools, or knowledge items by query. Supports full-text search with AND, OR, NOT operators, wildcards, and quoted phrases.

### Parameters

| Name    | Type    | Required | Default | Description                                            |
| ------- | ------- | -------- | ------- | ------------------------------------------------------ |
| `query` | string  | ✅       | —       | Search query                                           |
| `scope` | string  | ✅       | —       | Capability-format scope (see below)                    |
| `space` | string  | ❌       | `all`   | Space to search: `project`, `user`, `system`, or `all` |
| `limit` | integer | ❌       | `10`    | Maximum results to return                              |

### Scope Format

Scopes use capability string format or shorthand:

| Scope                   | Meaning                       |
| ----------------------- | ----------------------------- |
| `directive`             | All directives                |
| `tool`                  | All tools                     |
| `knowledge`             | All knowledge                 |
| `tool.rye.core.*`       | Tools under `rye/core/`       |
| `directive.rye.agent.*` | Directives under `rye/agent/` |

### Example

```python
rye_search(scope="tool", query="file system operations")
rye_search(scope="knowledge", query="metadata specification", space="system")
```

---

## `rye_load`

**Item ID:** `rye/primary/rye_load`

Load an item's full content for inspection. Can also copy items between spaces.

### Parameters

| Name          | Type   | Required | Default   | Description                                     |
| ------------- | ------ | -------- | --------- | ----------------------------------------------- |
| `item_type`   | string | ✅       | —         | `directive`, `tool`, or `knowledge`             |
| `item_id`     | string | ✅       | —         | Item ID (relative path without extension)       |
| `source`      | string | ❌       | —         | Space to load from: `project`, `user`, `system`. When omitted, cascades **project → user → system** (first match wins). |
| `destination` | string | ❌       | —         | Copy to this space: `project` or `user`         |

### Example

```python
# Inspect a directive
rye_load(item_type="directive", item_id="rye/core/create_directive")

# Copy a system tool to project space for customization
rye_load(item_type="tool", item_id="rye/file-system/read",
    source="system", destination="project")
```

---

## `rye_execute`

**Item ID:** `rye/primary/rye_execute`

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
rye_execute(item_type="tool", item_id="rye/bash/bash",
    parameters={"command": "echo test"}, dry_run=True)
```

---

## `rye_sign`

**Item ID:** `rye/primary/rye_sign`

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
