<!-- rye:unsigned -->

```yaml
name: completion
title: Completion Protocol
entry_type: context
category: rye/agent/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-24T00:00:00Z
tags:
  - completion
  - protocol
  - thread-started
```

## Completion Protocol

1. Execute all process steps using tool calls.
2. If the directive declares outputs, call `directive_return` with all required fields.
3. If blocked (permission denied, missing files, repeated failures), call `directive_return` with status=error immediately. Do NOT waste turns working around it.
4. Do NOT respond with text-only. Every response must contain tool calls until complete.
