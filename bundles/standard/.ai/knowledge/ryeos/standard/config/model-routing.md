<!-- ryeos:signed:2026-05-20T05:57:10Z:6487caec56ecbba2031a51daaf79160a23c0eb5a27682d0f2b1af8733d3a83dc:7UIBN3/FNm2wU+YUp7T8dWkJmjODa4HL7kV1rN+lqtHUs4bDGzM9KJPCpsMiuk8bd7WAbkbRqrDDeLQEyjZ7CA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config
tags: [config, models, routing, tiers]
version: "1.0.0"
description: Model routing config reference.
---

# Config: crates/core/runtime/model_routing

Invariant: model routing maps abstract directive tiers to concrete provider/model/context-window choices when a directive does not pin an explicit model.

The active standard table routes tiers through the `zen` provider. Directive metadata can override routing only by supplying a coherent explicit provider, model name, and context window.

Routing is resolved before runtime launch and included in the frozen provider snapshot.
