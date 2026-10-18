<!-- ryeos:signed:2026-10-17T05:23:22Z:84dd8eb506aedc3cb8fac00dfc2da77e57f1ef0f222600ab2acd461ef509160f:IdFkWcgOBi2IFxdZCTU3TC39HRcO75TdRvT+5Ac/EUE51CzaK0Of5qJ+MiEOy2VB9VdCdL3o74ExDJuSEDvYCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, commands, threads]
version: "1.0.0"
description: Commands service reference.
---

# Service: commands/submit

Invariant: commands/submit records an operator/runtime command against an active thread so the runner can react through the thread lifecycle machinery.

Commands include cancellation, kill, interrupt, and continue-style operations. The service is shipped by the core bundle; this reference lives in the ryeos-docs bundle because commands target runtime-managed thread execution.
