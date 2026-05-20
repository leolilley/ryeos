---
category: ryeos/core/services
tags: [service, signing, node]
version: "1.0.0"
description: Node-sign service reference.
---

# Service: node-sign

Invariant: node-sign signs daemon-owned node records with the node identity, not arbitrary bundle items.

Bundle items are signed by bundle publishers during publish. Node-sign is for daemon-internal state records whose trust root is the node identity.
