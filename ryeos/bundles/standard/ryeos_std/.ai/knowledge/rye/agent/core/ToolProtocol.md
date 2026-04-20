<!-- rye:signed:2026-04-19T09:49:53Z:64c83ebdc12ff3f4e386a148202a77fa851015139e9cb3b737d47dc03f446b4c:J2o1G3bZv+ZHhrGibRLU2Sc+Tp8jhcsTOstZ3tuHN+lOInXuuE5gL4Z3FMVdWJ2y+KGuzcB8fI+NqJy8fuB4Dw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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

You have three primary actions. They are the Rye OS interface.

### rye_execute — Run items

Execute a tool or directive. This is your primary action tool. Knowledge is not executable — use rye_fetch.

```json
rye_execute(item_id="rye/file-system/write", parameters={"path": "/abs/path", "content": "..."})
rye_execute(item_id="my/workflow", parameters={"target": "value"})
rye_fetch(item_id="project/design-spec")
```

### rye_fetch — Resolve or discover items

Unified item resolution. Two modes:

**ID mode** — resolve by exact path:
```json
rye_fetch(item_id="rye/file-system/write")
rye_fetch(item_type="directive", item_id="my/workflow")
```

**Query mode** — discover by keyword:
```json
rye_fetch(scope="tool", query="file system")
rye_fetch(scope="knowledge", query="design spec")
rye_fetch(scope="directive", query="build")
rye_fetch(scope="tool.rye.web.*", query="*")
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
rye_execute(item_id="rye/agent/threads/directive_return", parameters={"status": "completed", ...})
```

If blocked, return error immediately — do not waste turns:

```json
rye_execute(item_id="rye/agent/threads/directive_return", parameters={"status": "error", "error_detail": "what is missing"})
```

### Integrity errors

If execution fails with an IntegrityError, the error message tells you exactly what to do:
- **Unsigned item** → run `rye sign {type} {item_id}` (the exact command is in the error)
- **Content modified** → the file was edited after signing, re-sign it
- **Untrusted key** → the error lists all trusted key fingerprints

### Shadow detection

When `rye_fetch` returns results from multiple spaces, items may include:
- `shadows` — this item overrides the same item_id in a lower space
- `shadowed_by` — this item is overridden by a higher-precedence space

If a project tool shadows a system tool, fetch results make this visible.

### When unsure

If a directive tells you to use a tool and you don't know the exact item_id or parameters:
1. `rye_fetch` in query mode to find the item
2. `rye_fetch` in ID mode to inspect its schema
3. `rye_execute` to run it

Do NOT guess parameters. Look them up.
