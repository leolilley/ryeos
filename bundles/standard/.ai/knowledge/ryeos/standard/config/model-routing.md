<!-- ryeos:signed:2026-05-22T03:35:37Z:52a14d8992ae5196f72d531dfb91ba31284a7ae60a17f0bcc03522b4ec9ef9a7:QmEuWz3o1MQ2ctU7Ceao6Yl5qsZGnCAgUvW9WvTd1zEUhxDr8SbcY0MACEC/xuEqhFp9zLQUHFBIxEtGA4AlBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/standard/config
tags: [config, models, routing, tiers]
version: "1.0.0"
description: Model routing config reference.
---

# Config: ryeos-runtime/model_routing

Invariant: model routing maps abstract directive tiers to concrete provider/model/context-window choices when a directive does not pin an explicit model.

The active standard table routes tiers through the `zen` provider. Directive metadata can override routing only by supplying a coherent explicit provider, model name, and context window.

Routing is resolved before runtime launch and included in the frozen provider snapshot.
