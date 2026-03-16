<!-- rye:signed:2026-03-16T09:27:24Z:39db8f32a560d93c207981e205060fe59b5a4037e78740592295a327bac71aa1:Ol-tQ80pA-yoFSy6Wbl13V96uoIXDG5QCos8Bb9mVguGUFxVdlZkD2ebM2h2LCNiAJWYx2lu05eifYBXlqTQBQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: search
title: Search Protocol
entry_type: context
category: rye/agent/core/protocol
version: "1.0.0"
author: rye-os
created_at: 2026-03-04T00:00:00Z
tags:
  - tools
  - protocol
  - search
```

### rye_search — Discover items

Find item IDs before executing or loading them. Use when you don't know the exact ID.

```json
rye_search(scope="tool", query="file system")
rye_search(scope="knowledge", query="design spec")
rye_search(scope="directive", query="build")
rye_search(scope="tool.rye.web.*", query="*")
```

### Shadow detection

When `rye_search` returns results from multiple spaces, items may include:
- `shadows` — this item overrides the same item_id in a lower space
- `shadowed_by` — this item is overridden by a higher-precedence space

If a project tool shadows a system tool, search results make this visible.
