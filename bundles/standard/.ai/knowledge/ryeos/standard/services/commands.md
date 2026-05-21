<!-- ryeos:signed:2026-05-21T11:11:49Z:2575973b5ac3a4dda1a29c51ad7e2d24dab4b3003c6e1aaef832c1d826c87620:4FmCZ03uXHORQ6uy72OENEF0oLR45bFCvGAwSB/5n4R3EbD01Nu69YSWp/ZU7yKiOcQrKaeiNbNoZiI7SyefBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, commands, threads]
version: "1.0.0"
description: Commands service reference.
---

# Service: commands/submit

Invariant: commands/submit records an operator/runtime command against an active thread so the runner can react through the thread lifecycle machinery.

Commands include cancellation, kill, interrupt, and continue-style operations. The service belongs to the standard workflow layer because commands target runtime-managed thread execution.
