
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
