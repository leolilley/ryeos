<!-- rye:signed:2026-02-26T06:42:51Z:a0b0c3b2c22e18a93f77bfc3d09cf806365ba50fa7f5161e9f96d1cd4b2439e5:PEzS90PIMlBZDXIKAhk5iGkvsch5ZK6KmUeXW6seiYLFsEzpp8QlLbGqh67q3-IG_OluoWnoHlA6idq7W0zfBQ==:4b987fd4e40303ac -->
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

You have four primary tools. They are the Rye OS interface.

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

### When unsure

If a directive tells you to use a tool and you don't know the exact item_id or parameters:
1. `rye_search` to find the item
2. `rye_load` to inspect its schema
3. `rye_execute` to run it

Do NOT guess parameters. Look them up.
