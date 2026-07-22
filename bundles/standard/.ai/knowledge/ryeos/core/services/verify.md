<!-- ryeos:signed:2026-07-22T02:33:55Z:f52f7dff0c5805df14fe9f30ab4ab3479f0446318b1183b240cd74208cc8fcd7:yuqCym1ATLM1qXUKzd56PGpKt5AA+mmk++dg8xUhcvrPWktfZ+WRJN0WL7tchNY6v890BAZ2bRMobUIN2XSgDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, verify, signatures, offline]
version: "1.1.0"
description: Verify service reference.
---

# Service: verify

Invariant: `service:verify` resolves one or more items and checks signature integrity and publisher trust without running them.

Availability: **offline**. The CLI runs `verify` in-process using the engine's trust chain. No daemon is required.

Use verify to distinguish parse/resolution failures from trust failures before attempting execution.

```bash
ryeos --project <dir> verify <canonical-ref-or-.ai-path> [<canonical-ref-or-.ai-path>...]
```

A single target preserves the original single-report JSON shape. Multiple
targets return ordered `verified` and `failed` arrays and fail the command if
any target does not verify.

When the daemon is running, `verify` is also available as a daemon service endpoint for runtime-aware verification.
