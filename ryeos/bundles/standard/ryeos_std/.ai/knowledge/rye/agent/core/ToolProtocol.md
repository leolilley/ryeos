<!-- rye:signed:2026-03-16T11:23:45Z:07e66118676062c0af8a6e27ed9660d2ca3130ea2a99e4766b2f5b5cac05d7cd:dcYpEpDi_D-qtzbtawtsPdxUNt1TWJrpMN5mKyNbBE-4-KxZ7EHwDjcpnpCuiGySWtzEULt41MN7-Kv4KX7hCw==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: ToolProtocol
title: Tool Protocol
entry_type: context
category: rye/agent/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-24T00:00:00Z
tags:
  - tools
  - protocol
  - system-prompt
```

## Tool Protocol

You have four primary actions. They are the Rye OS interface.

### rye_execute — Run items

Execute a tool, directive, or knowledge item. This is your primary action tool.

```json
rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "/abs/path", "content": "..."})
rye_execute(item_type="directive", item_id="my/workflow", parameters={"target": "value"})
rye_execute(item_type="knowledge", item_id="project/design-spec")
```

### rye_search — Discover items

Find item IDs before executing or loading them. Use when you don't know the exact ID.

```json
rye_search(scope="tool", query="file system")
rye_search(scope="knowledge", query="design spec")
rye_search(scope="directive", query="build")
rye_search(scope="tool.rye.web.*", query="*")
```

### rye_load — Inspect items

Read raw content and metadata. Use to check a tool's parameter schema or a directive's inputs before executing.

```json
rye_load(item_type="tool", item_id="rye/file-system/write")
rye_load(item_type="directive", item_id="my/workflow")
```

### rye_sign — Sign items

Validate and sign items after editing. Required after any modification.

```json
rye_sign(item_type="directive", item_id="my/workflow")
rye_sign(item_type="tool", item_id="*")  // glob to batch sign
```

### Returning results

When the directive declares outputs, call directive_return:

```json
rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"status": "completed", ...})
```

If blocked, return error immediately — do not waste turns:

```json
rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"status": "error", "error_detail": "what is missing"})
```

### Integrity errors

If execution fails with an IntegrityError, the error message tells you exactly what to do:
- **Unsigned item** → run `rye sign {type} {item_id}` (the exact command is in the error)
- **Content modified** → the file was edited after signing, re-sign it
- **Untrusted key** → the error lists all trusted key fingerprints

### Shadow detection

When `rye_search` returns results from multiple spaces, items may include:
- `shadows` — this item overrides the same item_id in a lower space
- `shadowed_by` — this item is overridden by a higher-precedence space

If a project tool shadows a system tool, search results make this visible.

### When unsure

If a directive tells you to use a tool and you don't know the exact item_id or parameters:
1. `rye_search` to find the item
2. `rye_load` to inspect its schema
3. `rye_execute` to run it

Do NOT guess parameters. Look them up.
