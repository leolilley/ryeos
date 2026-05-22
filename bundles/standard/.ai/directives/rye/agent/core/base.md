---
description: "Rye agent base — extends general agent base with Rye identity and behavior"
version: "2.1.0"
extends: directive:agent/core/base
context:
  system:
    - knowledge:rye/agent/core/Identity
    - knowledge:rye/agent/core/Behavior
  suppress:
    - knowledge:agent/core/Behavior
permissions:
  execute:
    - "*"
  fetch:
    - "*"
  sign:
    - "*"
---

Standard operating context for Rye agent threads.
