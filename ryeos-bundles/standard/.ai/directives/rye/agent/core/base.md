---
description: "Rye agent base — extends general agent base with Rye identity and behavior"
version: "2.0.0"
context:
  system:
    - rye/agent/core/Identity
    - rye/agent/core/Behavior
  suppress:
    - agent/core/Behavior
permissions:
  execute:
    - "*"
  fetch:
    - "*"
  sign:
    - "*"
---

Standard operating context for Rye agent threads.
