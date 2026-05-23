<!-- ryeos:signed:2026-05-23T07:18:21Z:0f6fcd0389d29758b9c33ffd0193cc1994eed059228e12636958a2b07d681079:RoYVXMS2Hk6PJI60+r5xgvLAv5ftHM5+V4zUmQpccYKfQ/ZaZxHTJVzD3UbARNHWYgzyMbQDvITEs1Y40JRBDA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/core/services
tags: [service, fetch, resolution, offline]
version: "1.1.0"
description: Fetch service reference.
---

# Service: fetch

Invariant: `service:fetch` resolves an item through the engine and returns metadata/content without executing it.

Availability: **offline**. The CLI runs `fetch` in-process using the engine's resolution chain. No daemon is required.

```bash
ryeos fetch --item-ref <canonical-ref> --project-path <dir>
ryeos fetch --item-ref <canonical-ref> --project-path <dir> --with-content
ryeos fetch --item-ref <canonical-ref> --project-path <dir> --verify
```

The `--with-content` flag includes the full file body in the response.
The `--verify` flag also checks trust status and returns it alongside metadata.
