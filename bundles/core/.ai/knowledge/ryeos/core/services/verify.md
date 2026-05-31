<!-- ryeos:signed:2026-05-31T08:15:56Z:116f0262496f73c6e3b90fc45f29dabff1bc8308361a4be9dbc8d0755f78c111:1RFl5jYqB7i0B77v5xIVfzHCt85GmF/CLBQLye/vWWDr864UpoFy97qE+xnHvznIJ3OW89vLY47NaQlJ/8niDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
