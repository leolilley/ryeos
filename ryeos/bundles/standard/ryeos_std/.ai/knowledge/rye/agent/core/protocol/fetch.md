<!-- rye:signed:2026-03-29T06:39:14Z:ec0f47e3180513f6cb09a313cf11cca887cc5af0806f62b17e0bf824579a79fe:i_Kihv33GbS-hTwl2KvquBJN0sugJ9Dq0uFgyqz2TM_Yua8jL4nFjeIB1HJalnSbl7bBA9inaZy6yynj1SwGBQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: fetch
title: Fetch Protocol
entry_type: context
category: rye/agent/core/protocol
version: "1.0.0"
author: rye-os
created_at: 2026-03-04T00:00:00Z
tags:
  - tools
  - protocol
  - fetch
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

### Shadow detection

When query mode returns results from multiple spaces, items may include:
- `shadows` — this item overrides the same item_id in a lower space
- `shadowed_by` — this item is overridden by a higher-precedence space

If a project tool shadows a system tool, results make this visible.

### When unsure

If a directive tells you to use a tool and you don't know the exact item_id or parameters:
1. `rye_fetch` in query mode to find the item
2. `rye_fetch` in ID mode to inspect its schema
3. `rye_execute` to run it

Do NOT guess parameters. Look them up.
