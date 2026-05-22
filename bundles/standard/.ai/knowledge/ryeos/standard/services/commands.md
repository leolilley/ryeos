---
category: ryeos/standard/services
tags: [service, commands, threads]
version: "1.0.0"
description: Commands service reference.
---

# Service: commands/submit

Invariant: commands/submit records an operator/runtime command against an active thread so the runner can react through the thread lifecycle machinery.

Commands include cancellation, kill, interrupt, and continue-style operations. The service belongs to the standard workflow layer because commands target runtime-managed thread execution.
