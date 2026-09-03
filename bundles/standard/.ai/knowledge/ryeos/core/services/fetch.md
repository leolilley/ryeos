<!-- ryeos:signed:2026-08-18T22:04:51Z:96bd5abd8eba1795f243a664fb3d40042d29a7b9e395f78d46ceef591f3a5281:1ULShJpu4/d0dD69aMi9D8Csk/WAYMg29iUrCrQCxUUuIUfJS0QBSE2+M6A627b20nTMaY+7qn1UVml3s5SjDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, fetch, resolution, offline]
version: "1.2.0"
description: Fetch service reference.
---

# Service: fetch

Invariant: `service:fetch` resolves an item through the engine and returns metadata/content without executing it.

Availability: **offline**. The CLI runs `fetch` in-process using the engine's resolution chain. No daemon is required.

```bash
ryeos --project <dir> fetch --item-ref <canonical-ref>
ryeos --project <dir> fetch --item-ref <canonical-ref> --with-content
ryeos --project <dir> fetch --item-ref <canonical-ref> --verify
```

The `--with-content` flag includes the full file body in the response.
The `--verify` flag also checks trust status and returns it alongside metadata.
