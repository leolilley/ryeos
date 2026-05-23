<!-- ryeos:signed:2026-05-23T07:18:20Z:116f0262496f73c6e3b90fc45f29dabff1bc8308361a4be9dbc8d0755f78c111:yYWNkmYC+C8zBy3ii1M7kSmQkX+elcGkSncqCr+hvhMWVg9sKtgA7nhFQfnWqjJlO7ftDx3D+P37PIZ31qJ9Bg==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/core/services
tags: [service, verify, signatures, offline]
version: "1.1.0"
description: Verify service reference.
---

# Service: verify

Invariant: `service:verify` resolves an item and checks signature integrity and publisher trust without running it.

Availability: **offline**. The CLI runs `verify` in-process using the engine's trust chain. No daemon is required.

Use verify to distinguish parse/resolution failures from trust failures before attempting execution.

```bash
ryeos verify --item-ref <canonical-ref> --project-path <dir>
```

When the daemon is running, `verify` is also available as a daemon service endpoint for runtime-aware verification.
