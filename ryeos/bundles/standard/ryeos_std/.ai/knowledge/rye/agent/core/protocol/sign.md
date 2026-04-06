<!-- rye:signed:2026-04-06T04:14:32Z:3945e1f2a85e6ad91850fe481a39df77c3acec823f6f7756f9f7d9d5564fd1e4:339G9t3XjL9MQMZAjEVv8KOm5cNPOEho0pgf8Tq1qgKgbKERnhO0nFCU000G40RtTyOYN3-xRp_JpNBxTZTkAA:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: sign
title: Sign Protocol
entry_type: context
category: rye/agent/core/protocol
version: "1.0.0"
author: rye-os
created_at: 2026-03-04T00:00:00Z
tags:
  - tools
  - protocol
  - sign
```

### rye_sign — Sign items

Validate and sign items after editing. Required after any modification.

```json
rye_sign(item_type="directive", item_id="my/workflow")
rye_sign(item_type="tool", item_id="*")  // glob to batch sign
```
