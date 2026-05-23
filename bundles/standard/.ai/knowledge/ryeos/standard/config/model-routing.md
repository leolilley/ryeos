<!-- ryeos:signed:2026-05-22T19:55:06Z:52a14d8992ae5196f72d531dfb91ba31284a7ae60a17f0bcc03522b4ec9ef9a7:VSdCsMVe5bq6q570Tss97QOlU7ok1R0YvNE9fOIciIJJv6prL/EcUInRHkFnR1Lr+uWfVDENutUoJWGot8Z1BA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->

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
