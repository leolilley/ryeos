<!-- ryeos:signed:2026-10-17T01:32:58Z:14c08e26449b01d6a088b3d2a7ba71833a3904c68b99c4964aed1883aaf8b5d4:hoJzmR8MnucSIWpsChl1mVnBMJLf6IhR7xIoJUYs1VUoRyIawNth/Rhpy10K1+fH8O/sjSNYcIUk67M5wf/LDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, commands, threads]
version: "1.0.0"
description: Commands service reference.
---

# Service: commands/submit

Invariant: commands/submit records an operator/runtime command against an active thread so the runner can react through the thread lifecycle machinery.

Commands include cancellation, kill, interrupt, and continue-style operations. The service is shipped by the core bundle; this reference lives with the standard workflow knowledge because commands target runtime-managed thread execution.
