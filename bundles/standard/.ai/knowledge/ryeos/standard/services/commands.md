<!-- ryeos:signed:2026-05-23T09:45:40Z:2575973b5ac3a4dda1a29c51ad7e2d24dab4b3003c6e1aaef832c1d826c87620:YSMmAHH0vji86b/EWkasZuUCatp9G5H0RObfQnheFmSAJFh0sWQEv0mvgdTlQMX5rrAhaelkAv+IfXml2X1uAQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/services
tags: [service, commands, threads]
version: "1.0.0"
description: Commands service reference.
---

# Service: commands/submit

Invariant: commands/submit records an operator/runtime command against an active thread so the runner can react through the thread lifecycle machinery.

Commands include cancellation, kill, interrupt, and continue-style operations. The service belongs to the standard workflow layer because commands target runtime-managed thread execution.
