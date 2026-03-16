<!-- rye:signed:2026-03-16T09:53:45Z:c40abbbf0ff47a56d9dbbb8e0ad8bd881702e86cf2879e05a921b160ec1687f9:OfsD4csRcShcFYs7MwExQuobEhI22jFhkTB9x7jBgpLB6nuXdx2QinNEX9nMwLMZhEudmkxPkVIMC-fiwq7tAQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: load
title: Load Protocol
entry_type: context
category: rye/agent/core/protocol
version: "1.0.0"
author: rye-os
created_at: 2026-03-04T00:00:00Z
tags:
  - tools
  - protocol
  - load
```

### rye_load — Inspect items

Read raw content and metadata. Use to check a tool's parameter schema or a directive's inputs before executing.

```json
rye_load(item_type="tool", item_id="rye/file-system/write")
rye_load(item_type="directive", item_id="my/workflow")
```

### When unsure

If a directive tells you to use a tool and you don't know the exact item_id or parameters:
1. `rye_search` to find the item
2. `rye_load` to inspect its schema
3. `rye_execute` to run it

Do NOT guess parameters. Look them up.
