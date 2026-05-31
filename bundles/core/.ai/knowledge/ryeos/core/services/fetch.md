<!-- ryeos:signed:2026-05-31T08:15:56Z:0f6fcd0389d29758b9c33ffd0193cc1994eed059228e12636958a2b07d681079:xvaPJm/wSnHKgV5YxxZDLyC7ttntCqaizNGm2Qy5ybjNYHwcyAnP/Eo3gmpOdrf3F9RNh/o8veU5LqsTRU6RCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
