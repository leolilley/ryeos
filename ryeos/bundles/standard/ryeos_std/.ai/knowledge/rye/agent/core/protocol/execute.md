<!-- rye:signed:2026-04-09T00:11:21Z:4b3e43d6e150892b5c59c231a56fb4178c4f412c83536e15a9afb8be99d0621e:Z2KSSfW34b4UkKl8ORE7dzTBcjHPcgV4dX4hrQQBl2xYmirKhaJeEvLWXPDEPo7khqblWjffb_JPX66oaDNECA:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: execute
title: Execute Protocol
entry_type: context
category: rye/agent/core/protocol
version: "1.0.0"
author: rye-os
created_at: 2026-03-04T00:00:00Z
tags:
  - tools
  - protocol
  - execute
```

### rye_execute — Run items

Execute a tool, directive, or knowledge item. This is your primary action tool.

```json
rye_execute(item_id="rye/file-system/write", parameters={"path": "/abs/path", "content": "..."})
rye_execute(item_id="my/workflow", parameters={"target": "value"})
rye_fetch(item_id="project/design-spec")
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
